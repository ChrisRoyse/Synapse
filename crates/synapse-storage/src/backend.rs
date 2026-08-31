use std::{
    cmp::Ordering as CmpOrdering,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Seek, SeekFrom, Write},
    mem::size_of,
    ops::ControlFlow,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use calyx_aster::{
    cf::{ColumnFamily, KeyRange, prefix_range},
    durable_artifact,
    mmap_col::MmapColumn,
    mvcc::{CfRead, Freshness, LATEST_CF_RANGE_PAGE_MAX_ROWS, Snapshot, tombstone_value},
    wal,
};
use calyx_core::{Anchor, AnchorKind, AnchorValue, Constellation, CxId, TemporalPolicy, VaultId};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_calyx::{
    ACTION_CAUSAL_PREDICTOR_SLOTS, AsterOrphanSlotGcReport, SYNAPSE_CALYX_BASE_CF_WALK_PAGE_ROWS,
    SYNAPSE_CALYX_CF_WALK_PAGE_ROWS, SynapseCalyxAbundanceReport,
    SynapseCalyxAnchorBatchWriteReadback, SynapseCalyxAnchorReadback,
    SynapseCalyxAnchorWriteReadback, SynapseCalyxAssayParams,
    SynapseCalyxAtomicConstellationRecurrenceReadback, SynapseCalyxBackupReport,
    SynapseCalyxBitsReport, SynapseCalyxBlindSpotParams, SynapseCalyxBlindSpotReport,
    SynapseCalyxCausalMapReport, SynapseCalyxCausalViewRegistryReadback,
    SynapseCalyxCausalViewRegistryScope, SynapseCalyxCausalityReport, SynapseCalyxCfRangePage,
    SynapseCalyxCfRows, SynapseCalyxCfWalk, SynapseCalyxCfWrite, SynapseCalyxConditionalWriteError,
    SynapseCalyxConfig, SynapseCalyxDriftReport, SynapseCalyxEnsembleCardReport,
    SynapseCalyxErasureReport, SynapseCalyxError, SynapseCalyxFindParams, SynapseCalyxFindReport,
    SynapseCalyxGroundedObservationReadback, SynapseCalyxGroundingGapReport,
    SynapseCalyxGuardCalibrateParams, SynapseCalyxGuardCalibrateReport,
    SynapseCalyxGuardVerifyParams, SynapseCalyxGuardVerifyReport, SynapseCalyxHazardReport,
    SynapseCalyxKernelAnswerReport, SynapseCalyxKernelHealthReport, SynapseCalyxKernelParams,
    SynapseCalyxKernelRebuildParams, SynapseCalyxKernelRebuildReport, SynapseCalyxKernelReport,
    SynapseCalyxLedgerEntryReadback, SynapseCalyxLedgerVerifyReport,
    SynapseCalyxMultiConditionalWriteOutcome, SynapseCalyxObservationPutReadback,
    SynapseCalyxPanelDriftParams, SynapseCalyxPanelDriftReport, SynapseCalyxPanelState,
    SynapseCalyxPeriodicityReport, SynapseCalyxPersistedNoveltyFinding,
    SynapseCalyxPersistedRecurrenceFinding, SynapseCalyxPersistedRegionFinding,
    SynapseCalyxReadOnlyVault, SynapseCalyxRecurrenceAppendReadback,
    SynapseCalyxRecurrenceSeriesReadback, SynapseCalyxRedundancyReport,
    SynapseCalyxReproduceReport, SynapseCalyxRetiredSearchGeneration, SynapseCalyxRevisionGuard,
    SynapseCalyxSearchCommissionParams, SynapseCalyxSearchCommissionReport,
    SynapseCalyxSearchRebuildReport, SynapseCalyxSnapshotGcObservation,
    SynapseCalyxSufficiencyReport, SynapseCalyxTemporalCandidate, SynapseCalyxTemporalParams,
    SynapseCalyxTemporalRerankReadback, SynapseCalyxVault, SynapseCalyxVaultCloseReadback,
    SynapseCalyxVaultStatus, SynapseCalyxVaultVerifyReport, SynapseCalyxVerifyReport,
    SynapseCalyxWalkStep, SynapseCalyxWeaveParams, SynapseCalyxWeaveReport,
    VaultTemporalPanelRegistration,
};
use synapse_core::{
    error_codes,
    retention::{DEFAULTS, RetentionCapEviction, RetentionDefault, RetentionTtl},
    types::{
        AgentEventRecord, AgentTranscriptRecord, EpisodeRecord, StoredObservation,
        StoredReflexAudit, TimelineRecord,
    },
};

use crate::constellations::{
    ConstellationPutReport, NativeConstellationContext, RecurrenceSubjectKind,
    SYN_ACTION_PANEL_NAME, SYN_ACTION_PANEL_VERSION, SYN_AGENT_EVENT_PANEL_NAME,
    SYN_AGENT_EVENT_PANEL_VERSION, SYN_AGENT_TRANSCRIPT_PANEL_NAME,
    SYN_AGENT_TRANSCRIPT_PANEL_VERSION, SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION,
    SYN_MCP_USAGE_BACKFILL_SOURCE, SYN_MCP_USAGE_KEY_PREFIX, SYN_MCP_USAGE_PANEL_NAME,
    SYN_MCP_USAGE_PANEL_VERSION, SYN_OBSERVATION_PANEL_NAME, SYN_OBSERVATION_PANEL_VERSION,
    SYN_OUTCOME_BACKFILL_SOURCE, SYN_OUTCOME_PANEL_NAME, SYN_OUTCOME_PANEL_VERSION,
    SYN_PROCESS_PANEL_NAME, SYN_PROCESS_PANEL_VERSION, SYN_RECURRENCE_SUBJECT_PANEL_NAME,
    SYN_RECURRENCE_SUBJECT_PANEL_VERSION, SYN_REFLEX_PANEL_NAME, SYN_REFLEX_PANEL_VERSION,
    SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION, SupersededPanelLineage,
    assert_panel_carries_graded_dense_lens, assert_syn_lens_provenance_complete,
    superseded_panel_lineage, syn_queryable_panel_contract, syn_reconstructable_panel_contract,
};
use crate::{
    CfEstimateMap, CfRevisionGuard, CoherentScanLease, CoherentScanScope, FixedWidthScanPage,
    OwnedCfWriteBatch, OwnedCfWriteBatchWithExpiry, PhysicalScanPage, RawRow, RawRowWithExpiry,
    RevisionGuard, RevisionGuardConflict, RevisionGuardedMutationOutcome,
    RevisionGuardedWriteOutcome, RevisionedRawValue, STORAGE_REVISION_GUARD_INVALID,
    STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE, STORAGE_REVISION_GUARDED_OUTCOME_INVALID, ScanWindow,
    StorageError, StorageResult, cf, constellations, gc, pressure,
};

const MIB: usize = 1024 * 1024;
const SCHEMA_VERSION_KEY: &[u8] = b"__schema_version";
const STORAGE_WRITES_SHED_TOTAL: &str = "storage_writes_shed_total";
const STORAGE_CF_BYTES: &str = "storage_cf_bytes";
const CALYX_KV_DISC: u8 = 0x03;
const CALYX_KV_VALUE_VERSION_V1: u8 = 0x01;
const CALYX_KV_VALUE_VERSION: u8 = 0x02;
const CALYX_KV_VALUE_V1_HEADER_BYTES: usize = 1 + 8;
const CALYX_KV_VALUE_HEADER_BYTES: usize = 1 + 8 + 8;
const CALYX_MAX_USER_KEY_BYTES: usize = u16::MAX as usize;
const CALYX_KV_LEGACY_LENGTH_ORDERED_NAMESPACE: u64 = 0;
const CALYX_KV_ORDERED_NAMESPACE: u64 = 1;
const CALYX_COLLECTION_ID_BASE: u64 = 0x5359_4e43_4600_0000;
const CALYX_METADATA_COLLECTION_ID: u64 = CALYX_COLLECTION_ID_BASE | 0xffff;
const CALYX_ORDERED_KEY_MIGRATION_KEY: &[u8] = b"__ordered_key_namespace_v1";
const CALYX_ORDERED_KEY_MIGRATION_PAYLOAD: &[u8] = b"synapse-calyx-ordered-key-namespace-v1";
const CALYX_ORDERED_KEY_PHYSICAL_PURGE_KEY: &[u8] = b"__ordered_key_namespace_v1_physical_purge";
const CALYX_ORDERED_KEY_PHYSICAL_PURGE_PAYLOAD: &[u8] =
    b"synapse-calyx-ordered-key-namespace-v1-physical-purge";
const CALYX_ORDERED_KEY_MIGRATION_PAGE_ROWS: usize = 512;
const CALYX_GC_CF: &str = "storage_gc";
const CALYX_GC_WAL_RECYCLE_MAX_SEGMENTS: usize = 8;
const CALYX_GC_WAL_RECYCLE_FSYNC_BUDGET: usize = 8;
const CALYX_CHECKPOINT_CF: &str = "storage_checkpoint";
/// Cadence of the checkpoint-only maintenance task (2026-07-23 cold-start
/// fix). Bounds the crash-stranded WAL tail — and therefore restart WAL
/// replay plus idempotent SST republish — to at most one interval of commits,
/// instead of one 5-minute GC interval (~20k sequences / minutes of replay
/// observed). Each tick's cost is proportional to the commits staged since the
/// previous tick plus a single manifest publish.
const CALYX_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
/// Tombstone-purge pacing: a full-CF streaming tombstone purge rewrites the
/// entire KV CF (hundreds of MB) with an unlimited compaction throttle, so it
/// must not fire on every GC tick just because a handful of rows were evicted.
/// It runs only once the deferred logical-delete backlog crosses this row
/// floor, or the max-defer interval below elapses. Deferring is always safe:
/// the tombstones are already committed logical deletes; only physical space
/// reclamation is postponed, and urgent reclamation still flows through the
/// disk-pressure compaction path.
const CALYX_GC_TOMBSTONE_PURGE_ROW_THRESHOLD: u64 = 4_096;
/// Upper bound on how long a non-empty tombstone backlog may be deferred before
/// a full-CF purge is forced regardless of the row floor.
const CALYX_GC_TOMBSTONE_PURGE_MAX_DEFER_MS: u64 = 30 * 60 * 1_000;
/// Deferred (committed but not yet physically purged) KV tombstone rows.
static CALYX_GC_PENDING_TOMBSTONE_ROWS: AtomicU64 = AtomicU64::new(0);
/// Vault-clock millisecond stamp of the last full-CF tombstone purge, `0` until
/// the first purge runs.
static CALYX_GC_LAST_TOMBSTONE_PURGE_MS: AtomicU64 = AtomicU64::new(0);
const CALYX_GC_PROTECTED_CF_POLICY_SKIPPED: &str = "protected_cf_policy_skipped";
const CALYX_GC_CACHE_EVICTIONS_TOTAL: &str = "cache_evictions_total";
/// GC kept a source row alive because a live derived constellation is
/// content-addressed over its bytes and could not be re-derived without it
/// (#1882).
const CALYX_GC_SOURCE_ROW_RETAINED_FOR_DERIVED: &str =
    "STORAGE_CALYX_GC_SOURCE_ROW_RETAINED_FOR_DERIVED";
/// Process-local historical readers are deliberately few and short-lived.
/// Each one pins MVCC versions that would otherwise be reclaimable, so an
/// unbounded lease table is an unbounded memory-retention contract.
const CALYX_STORAGE_SNAPSHOT_MAX_ACTIVE: usize = 64;
pub const CALYX_STORAGE_SNAPSHOT_MIN_AGE_MS: u64 = 100;
pub const CALYX_STORAGE_SNAPSHOT_MAX_AGE_MS: u64 = 60_000;
const CALYX_GC_SOFT_CAP_REASON: &str = "soft_cap";
const CALYX_WRITE_BATCH_ROW_COUNT_BYTES: usize = 4;
const CALYX_WRITE_BATCH_CF_TAG_BYTES: usize = 1;
const CALYX_WRITE_BATCH_LEN_PREFIX_BYTES: usize = 4;
const CALYX_WRITE_BATCH_ROW_OVERHEAD_BYTES: usize =
    CALYX_WRITE_BATCH_CF_TAG_BYTES + (2 * CALYX_WRITE_BATCH_LEN_PREFIX_BYTES);
const CALYX_WRITE_BATCH_WAL_HEADROOM_BYTES: usize = MIB;
const CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES: usize =
    wal::MAX_RECORD_BYTES - CALYX_WRITE_BATCH_WAL_HEADROOM_BYTES;
const MILLIS_PER_HOUR: u64 = 60 * 60 * 1_000;
const MILLIS_PER_DAY: u64 = 24 * MILLIS_PER_HOUR;
const MIB_U64: u64 = 1024 * 1024;
pub const STORAGE_METADATA_ONLY_REDACTION_POLICY: &str =
    "metadata_only_no_raw_keys_or_values_hashes_for_correlation";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageCfDump {
    pub backend: StorageBackendKind,
    pub cf_name: String,
    pub row_count: u64,
    pub rows: Vec<StorageDumpRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageDumpRow {
    pub key_len_bytes: u64,
    pub key_sha256: String,
    pub key_material_omitted: bool,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub value_encoding: String,
    pub value_content_omitted: bool,
    pub redaction_policy: String,
}

struct PendingConstellationReport {
    source_key: Vec<u8>,
    raw_bytes: Vec<u8>,
    slot_count: u64,
    scalar_count: u64,
}

struct ConstellationBatch {
    constellations: Vec<Constellation>,
    pending_reports: Vec<PendingConstellationReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalyxVaultInspect {
    pub schema_version: u32,
    pub vault_id: String,
    pub latest_seq: u64,
    /// Physical in-memory MVCC reclamation state read independently of a GC
    /// trigger. Cumulative totals are monotonic for this process lifetime.
    pub snapshot_gc: SynapseCalyxSnapshotGcObservation,
    pub inspected_at_unix_ms: u64,
    pub collection_count: u64,
    pub raw_row_count: u64,
    pub live_row_count: u64,
    pub expired_row_count: u64,
    pub user_key_bytes: u64,
    pub payload_bytes: u64,
    pub stored_value_bytes: u64,
    pub total_logical_bytes: u64,
    /// Pages the bounded-hold census actually read (#2041).
    pub census_pages: u64,
    /// Candidate rows requested per page.
    pub census_page_rows: u64,
    /// Committed sequence serving the census's first page.
    pub census_snapshot_seq_first: u64,
    /// Committed sequence serving the census's last page.
    pub census_snapshot_seq_last: u64,
    /// Whether every page was served by the same committed sequence.
    ///
    /// True means the totals below describe one instant, exactly as the
    /// single-acquisition scan this replaced always did. False means a commit
    /// landed mid-census and the totals describe an interval — reported rather
    /// than silently presented as an instant.
    pub census_atomic: bool,
    pub collections: BTreeMap<String, CalyxVaultCollectionInspect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalyxVaultCollectionInspect {
    pub collection_name: String,
    pub cf_name: Option<String>,
    pub collection_id_hex: String,
    pub namespace: u64,
    pub raw_row_count: u64,
    pub live_row_count: u64,
    pub expired_row_count: u64,
    pub user_key_bytes: u64,
    pub payload_bytes: u64,
    pub stored_value_bytes: u64,
    pub total_logical_bytes: u64,
    pub expires_at_ms_histogram: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroundingAnchorValue {
    Bool(bool),
    Enum(String),
    Number(f64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroundingAnchor {
    pub kind_label: String,
    pub value: GroundingAnchorValue,
    pub source: String,
    pub observed_at_ms: u64,
    pub confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalyxAnchorWriteReport {
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_value_sha256: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub cx_id: String,
    pub anchor_kind: String,
    pub anchor_value: CalyxAnchorValueReadback,
    pub anchor_source: String,
    pub confidence: f32,
    pub ledger_seq: u64,
    pub ledger_hash: String,
    pub latest_seq: u64,
    pub readback_anchor_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpUsageGroundedPublicationReport {
    pub source_row_count: u64,
    pub source_readback_exact_match_count: u64,
    pub source_key_hex: String,
    pub source_value_len_bytes: u64,
    pub source_value_sha256: String,
    pub committed_seq: u64,
    pub constellation: ConstellationPutReport,
    pub anchor: CalyxAnchorWriteReport,
}

/// Physical readback from one atomic reflex-registration publication.
#[derive(Clone, Debug, PartialEq)]
pub struct ReflexRegistrationPublicationReport {
    pub source_row_count: u64,
    pub source_readback_exact_match_count: u64,
    pub source_key_hex: String,
    pub source_value_len_bytes: u64,
    pub source_value_sha256: String,
    pub committed_seq: u64,
    pub constellation: ConstellationPutReport,
    pub anchor: CalyxAnchorWriteReport,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReflexGroundedLifecycleMember {
    pub source_key: Vec<u8>,
    pub raw_bytes: Vec<u8>,
    pub record: StoredReflexAudit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflexLifecycleBatchPublicationReport {
    pub member_count: u64,
    pub source_row_count: u64,
    pub source_readback_exact_match_count: u64,
    pub committed_seq: u64,
    pub cx_ids: Vec<String>,
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CalyxRecurrenceSubjectReport {
    pub subject_kind: String,
    pub subject_id: String,
    pub subject_cx_id: String,
    pub subject_panel_name: String,
    pub subject_panel_version: u32,
    pub subject_disposition: synapse_calyx::SynapseCalyxPutDisposition,
    pub occurrence: SynapseCalyxRecurrenceAppendReadback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionOraclePublicationReport {
    pub subject_cx_id: String,
    pub constellation_cx_id: String,
    pub occurrence_id: u64,
    pub committed_seq: u64,
    pub latest_seq: u64,
    pub source_row_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PanelLifecycleBackfillReport {
    pub panel_name: String,
    pub source_panel_version: u32,
    pub target_panel_version: u32,
    pub claimed: u64,
    pub recovered_in_flight: u64,
    pub completed: u64,
    pub pending: u64,
    pub in_flight: u64,
    pub completed_total: u64,
    pub registry_committed_seq: u64,
    pub registry_value_sha256: String,
    pub verified_cx_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroundingAnchorSource {
    pub source_cf: &'static str,
    pub source_key: Vec<u8>,
    pub raw_bytes: Vec<u8>,
    pub anchor: GroundingAnchor,
}

struct PreparedGroundingAnchorSource {
    source_cf: &'static str,
    source_key: Vec<u8>,
    cx_id: CxId,
    anchor: Anchor,
}

type PreparedGroundingAnchorBatch = (Vec<PreparedGroundingAnchorSource>, Vec<(CxId, Vec<Anchor>)>);

#[derive(Clone, Copy)]
struct TemporalBackfillBuildContext<'a> {
    source_cf: &'a str,
    active_panel_version: u32,
    vault_id: VaultId,
    created_at_ms: u64,
    next_ledger_seq: u64,
}

struct PreparedTranscriptBackfillOutcome {
    anchor: Option<GroundingAnchor>,
    unadjudicable: bool,
}

struct PreparedTemporalBackfillRow {
    constellation: Constellation,
    measurement_plan: constellations::DeferredMeasurementPlan,
    transcript_outcome: Option<PreparedTranscriptBackfillOutcome>,
    action_outcome_present: Option<bool>,
}

struct TemporalBackfillRowSidecars {
    transcript_outcome: Option<PreparedTranscriptBackfillOutcome>,
    action_outcome_present: Option<bool>,
}

struct TemporalBackfillRowOutcome {
    put: SynapseCalyxObservationPutReadback,
    migration: Option<calyx_aster::vault::TemporalMetadataMigration>,
    cx_id: CxId,
    sidecars: TemporalBackfillRowSidecars,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalyxAnchorBatchWriteReport {
    pub requested_anchor_count: u64,
    pub written_anchor_count: u64,
    pub existing_anchor_count: u64,
    pub readback_exact_match_count: u64,
    pub ledger_seq: Option<u64>,
    pub ledger_hash: Option<String>,
    pub latest_seq: u64,
    pub duration_us: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalyxAnchorRow {
    pub key_hex: String,
    pub cx_id: String,
    pub kind: String,
    pub value: CalyxAnchorValueReadback,
    pub source: String,
    pub observed_at_ms: u64,
    pub confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalyxAnchorScanReport {
    pub source_cf: String,
    pub source_key_hex: String,
    pub source_value_sha256: String,
    pub panel_name: String,
    pub panel_version: u32,
    pub cx_id: String,
    pub anchors: Vec<CalyxAnchorRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalyxAnchorValueReadback {
    pub value_type: String,
    pub bool_value: Option<bool>,
    pub text_value: Option<String>,
    pub number_value: Option<f64>,
    pub one_hot_values: Vec<String>,
    pub vector_len: Option<u64>,
    pub vector_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackendKind {
    #[default]
    Calyx,
}

impl StorageBackendKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Calyx => "calyx",
        }
    }

    /// Parses a daemon storage-backend config value.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::BackendInvalidConfig`] when `value` is not one
    /// of the accepted backend names.
    pub fn parse_config(value: &str) -> StorageResult<Self> {
        Self::from_str(value).map_err(|detail| StorageError::BackendInvalidConfig {
            value: value.to_owned(),
            detail,
        })
    }
}

impl FromStr for StorageBackendKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "calyx" => Ok(Self::Calyx),
            other => Err(format!("storage_backend must be \"calyx\"; got {other:?}")),
        }
    }
}

/// One lens pair from a synergy pass (#1672, #1941).
///
/// `gain_bits` is `max(0, pair_bits - max(left_bits, right_bits))` — the bits
/// the pair carries about the outcome beyond what its better half carries alone
/// (`WholeMinusMax`, Griffith & Koch arXiv:1205.4265), floored by the
/// data-processing inequality. `raw_gain_bits` keeps the unclamped difference
/// and `monotonicity_floor_applied` marks a row the floor had to move, so a
/// clamped row is visibly clamped (#1941).
///
/// All three terms come from one estimator, named per component so the caller
/// can see it rather than assume it. A pair whose three columns could not be
/// measured by any single instrument is refused, not reported: `state` says
/// which refusal and `unmeasured_reason` says why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseSynergyPair {
    pub slot_a: u16,
    pub slot_b: u16,
    pub pair_bits: f32,
    pub left_bits: f32,
    pub right_bits: f32,
    pub gain_bits: f32,
    pub raw_gain_bits: f32,
    pub monotonicity_floor_applied: bool,
    /// The instrument behind `pair_bits`; `None` when the pair is unmeasured.
    pub pair_estimator: Option<String>,
    /// The instrument behind `left_bits`; `None` when the pair is unmeasured.
    pub left_estimator: Option<String>,
    /// The instrument behind `right_bits`; `None` when the pair is unmeasured.
    pub right_estimator: Option<String>,
    pub n_samples: usize,
    pub synergistic: bool,
    pub provisional: bool,
    /// `measured` | `insufficient_samples` | `estimator_refused` |
    /// `cross_estimator_unpinnable`.
    pub state: String,
    pub unmeasured_reason: Option<String>,
    /// The halves of this pair that are declared anchor source carriers, in
    /// slot order (#1959).
    ///
    /// Non-empty means the concatenated column this pair's `pair_bits` was
    /// measured over **contains the label**, so `gain_bits` is not a statement
    /// about prediction. Unlike a bits report there is no per-slot row here for
    /// a reader to notice that in, which is why the pair carries it itself.
    pub anchor_source_carrier_slots: Vec<u16>,
}

/// Result of one Assay synergy pass, with the physical Assay CF readback and
/// the #1670 domain grounding verdict the result must be read under.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseSynergyReport {
    pub panel_version: u32,
    pub anchor_kind: String,
    pub anchored_records: usize,
    /// Lenses the panel carries.
    pub n_lenses: usize,
    /// Lenses actually paired under the bounded synergy budget.
    pub lenses_paired: usize,
    pub pairs_evaluated: usize,
    /// Pairs reported without a measured gain, for any reason (#1941).
    pub pairs_unmeasured: usize,
    /// Pairs refused because no single instrument could measure all three
    /// terms, so their difference would not have been a measurement (#1941).
    pub pairs_cross_estimator_unpinnable: usize,
    /// Measured pairs whose raw gain was negative and was floored at zero by
    /// the data-processing inequality (#1941).
    pub pairs_monotonicity_floored: usize,
    pub synergistic_pairs: usize,
    pub max_gain_bits: f32,
    /// [`Self::max_gain_bits`] over the pairs containing no declared anchor
    /// source carrier (#1959) — the headline that is a claim about prediction.
    pub max_gain_bits_carrier_free: f32,
    /// Whether this (anchor kind, panel version) pair declares its determining
    /// record fields, so the structural carrier check could run at all.
    /// `false` means it did not run, not that it ran and found nothing.
    pub anchor_source_declared: bool,
    /// Panel slots whose declared source fields intersect the anchor's.
    pub anchor_source_carriers: Vec<SynapseAnchorSourceCarrier>,
    /// Evaluated pairs with at least one carrier half.
    pub pairs_with_anchor_source_carrier: usize,
    /// Control-doctrine marker (#1670): domain anchor coverage below the floor.
    pub domain_provisional: bool,
    pub domain_grounded_fraction: f32,
    pub pairs: Vec<SynapseSynergyPair>,
    pub assay_cf_rows_after: usize,
}

/// One lens whose declared source fields intersect the anchor's determining
/// fields — it reads the label rather than evidence about it (#1958, #1959).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseAnchorSourceCarrier {
    pub slot: u16,
    pub lens: String,
    pub shared_fields: Vec<String>,
}

impl From<synapse_calyx::SynapseCalyxAnchorSourceCarrier> for SynapseAnchorSourceCarrier {
    fn from(carrier: synapse_calyx::SynapseCalyxAnchorSourceCarrier) -> Self {
        Self {
            slot: carrier.slot,
            lens: carrier.lens,
            shared_fields: carrier.shared_fields,
        }
    }
}

pub trait StorageBackend: Send + Sync {
    fn kind(&self) -> StorageBackendKind;
    fn put_batch(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()>;
    fn put_batch_pressure_bypass(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()>;
    fn put_cf_batches_pressure_bypass(&self, batches: Vec<OwnedCfWriteBatch>) -> StorageResult<()>;
    fn put_cf_batches_if_revisions_pressure_bypass(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
    ) -> StorageResult<RevisionGuardedMutationOutcome>;
    fn put_cf_batches_with_expiry_if_revisions_pressure_bypass(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatchWithExpiry>,
    ) -> StorageResult<RevisionGuardedMutationOutcome>;
    fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>>;
    fn get_cf_revisioned(
        &self,
        cf_name: &str,
        key: &[u8],
    ) -> StorageResult<Option<RevisionedRawValue>>;
    fn open_calyx_storage_snapshot(
        &self,
        max_age_ms: u64,
    ) -> StorageResult<CalyxStorageSnapshotLease>;
    fn read_calyx_storage_snapshot(
        &self,
        lease_id: u64,
        cf_name: &str,
        key: &[u8],
    ) -> StorageResult<CalyxStorageSnapshotReadback>;
    fn release_calyx_storage_snapshot(
        &self,
        lease_id: u64,
    ) -> StorageResult<CalyxStorageSnapshotRelease>;
    fn put_batch_if_revision_pressure_bypass(
        &self,
        cf_name: &str,
        guard_key: &[u8],
        expected_revision_sha256: Option<[u8; 32]>,
        rows: Vec<RawRow>,
    ) -> StorageResult<RevisionGuardedWriteOutcome>;
    fn mutate_batch_if_revisions_pressure_bypass(
        &self,
        cf_name: &str,
        guards: Vec<RevisionGuard>,
        deletes: Vec<Vec<u8>>,
        puts: Vec<RawRow>,
    ) -> StorageResult<RevisionGuardedMutationOutcome>;
    fn mutate_batch_pressure_bypass(
        &self,
        cf_name: &str,
        deletes: Vec<Vec<u8>>,
        puts: Vec<RawRow>,
    ) -> StorageResult<()>;
    fn delete_batch(&self, cf_name: &str, keys: Vec<Vec<u8>>) -> StorageResult<()>;
    fn flush(&self) -> StorageResult<()>;
    fn run_gc_once(&self) -> StorageResult<gc::GcReport>;
    fn run_gc_once_with_row_caps(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<gc::GcReport>;
    fn spawn_gc_task(&self) -> StorageResult<gc::GcTask>;
    fn spawn_checkpoint_task(&self) -> StorageResult<gc::GcTask>;
    fn spawn_derived_state_task(&self) -> StorageResult<gc::GcTask>;
    /// Keeps **every** published search generation inside its reconciliation
    /// bound, not only the active panel's (#1938).
    fn maintain_calyx_search_generation(
        &self,
    ) -> StorageResult<crate::search_sweep::SearchGenerationSweep>;
    fn measure_calyx_lens_coverage(
        &self,
        max_records: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxLensCoverageStatus>;
    /// Per-panel coverage and grounding census (#1927 ask 1, #1920 ask 1).
    fn measure_panel_coverage(&self) -> StorageResult<crate::panel_coverage::PanelCoverageReport>;
    fn olap_aggregate_slot(
        &self,
        panel_version: u32,
        slot_id: u32,
        value_column: usize,
        group_by_column: Option<usize>,
        max_rows: usize,
        max_groups: usize,
    ) -> StorageResult<synapse_calyx::olap::OlapScanResult>;
    fn ensure_timeseries_collection(&self, collection_name: &str) -> StorageResult<()>;
    fn timeseries_write(
        &self,
        collection_name: &str,
        series: u64,
        timestamp_ns: u64,
        value: f64,
    ) -> StorageResult<u64>;
    fn timeseries_rollup(
        &self,
        collection_name: &str,
        series: u64,
        window: synapse_calyx::timeseries::SynapseCalyxRollupWindow,
        timestamp_ns: u64,
    ) -> StorageResult<Option<synapse_calyx::timeseries::SynapseCalyxRollupValue>>;
    fn timeseries_range(
        &self,
        collection_name: &str,
        series: u64,
        start_timestamp_ns: u64,
        end_timestamp_ns: u64,
    ) -> StorageResult<Vec<(u64, f64)>>;
    fn pressure_level(&self) -> pressure::DiskPressureLevel;
    fn pressure_permits_write(&self, cf_name: &str) -> bool;
    fn pressure_transition_codes(&self) -> StorageResult<Vec<&'static str>>;
    fn pressure_probe_readback(&self) -> StorageResult<pressure::PressureProbeReadback>;
    fn cf_sizes(&self) -> StorageResult<BTreeMap<String, u64>>;
    fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap>;
    fn cf_row_counts(&self) -> StorageResult<BTreeMap<String, u64>>;
    fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap>;
    fn snapshot_gc_observation(&self) -> StorageResult<SynapseCalyxSnapshotGcObservation>;
    fn calyx_vault_status(&self) -> StorageResult<SynapseCalyxVaultStatus>;
    /// Read-only state of the persisted search generation for the active panel
    /// (issue #1891). Side-effect free, so `health` can report it every call.
    fn calyx_search_generation_status(
        &self,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchGenerationStatus>;
    /// The same state for one **named** panel generation, active or not, with
    /// the changed-key delta optionally measured (#1938).
    ///
    /// `measure_delta = true` is the expensive path: it scans the `Base` CF and
    /// every indexed slot CF exactly as the query path's delta collector does,
    /// so the number is the one a query will be judged against rather than a
    /// proxy for it. Never call it from a request path.
    fn calyx_search_generation_status_for_panel(
        &self,
        panel_version: u32,
        measure_delta: bool,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchGenerationStatus>;
    /// One constellation's `Base` and slot row MVCC sequences (#1935), the fact
    /// set that identifies a writer which staged a slot row without its own
    /// `Base` row.
    fn diagnose_constellation_row_sequences(
        &self,
        cx_id: calyx_core::CxId,
    ) -> StorageResult<synapse_calyx::ConstellationRowSequences>;
    /// Keys one native CF's MVCC changed-key history reports after `after_seq`
    /// (#1935), for comparison against that CF's plain row count.
    fn calyx_changed_key_count_after(&self, cf_name: &str, after_seq: u64) -> StorageResult<u64>;
    /// Rows one native Calyx CF holds at the latest snapshot (#1935).
    fn calyx_cf_row_count(&self, cf_name: &str) -> StorageResult<u64>;
    /// Publishes the lowered guard-threshold artifact through the vault's own
    /// single producer (#1885).
    ///
    /// The maintenance publisher used to rebuild the fingerprinted envelope by
    /// hand because the sole `Arc<SynapseCalyxVault>` lives behind a private
    /// `with_vault`. Two independent producers of one on-disk format is one
    /// too many, so the vault's producer is exposed here instead and the
    /// reconstruction is gone.
    fn lower_guard_thresholds(
        &self,
        params: &synapse_calyx::LoweringParams,
    ) -> StorageResult<synapse_calyx::LoweredPublishReport>;
    fn rebuild_calyx_search_indexes(
        &self,
        expected_panel_version: u32,
    ) -> StorageResult<SynapseCalyxSearchRebuildReport>;
    fn propose_calyx_search_tuning(
        &self,
        expected_panel_version: u32,
        candidate: synapse_calyx::SynapseCalyxTuningConfig,
        description: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAnnealSearchReport>;
    fn rollback_calyx_anneal(
        &self,
        change_id: u64,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAnnealRollbackReport>;
    fn find_similar(
        &self,
        params: &SynapseCalyxFindParams,
    ) -> StorageResult<SynapseCalyxFindReport>;
    fn retire_orphan_slot_cfs(&self) -> StorageResult<AsterOrphanSlotGcReport>;
    fn retire_search_generation(
        &self,
        panel_version: u32,
    ) -> StorageResult<(SynapseCalyxRetiredSearchGeneration, SupersededPanelLineage)>;
    fn close_calyx_vault(
        &self,
        reason: &'static str,
    ) -> StorageResult<SynapseCalyxVaultCloseReadback>;
    fn close_calyx_vault_for_process_exit(
        &self,
        reason: &'static str,
    ) -> StorageResult<SynapseCalyxVaultCloseReadback>;
    fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>>;
    fn backup_calyx_vault(
        &self,
        target_root: &Path,
        include_regenerable: bool,
    ) -> StorageResult<SynapseCalyxBackupReport>;
    fn verify_calyx_restore(&self, vault_path: &Path) -> StorageResult<SynapseCalyxVerifyReport>;
    /// Scheduled whole-vault verification of the *live* vault: restore verifier
    /// plus provenance chain, under the vault maintenance guard. The chain scan
    /// is incremental over the newest `tail_entries` unless `full_chain`.
    fn verify_calyx_vault(
        &self,
        full_chain: bool,
        tail_entries: u64,
    ) -> StorageResult<SynapseCalyxVaultVerifyReport>;
    /// Verifies the live provenance-ledger hash chain against the stored bytes.
    /// `range` is an optional half-open `(from_seq, to_seq)` window; `None`
    /// verifies the full chain.
    fn verify_calyx_ledger_chain(
        &self,
        range: Option<(u64, u64)>,
    ) -> StorageResult<SynapseCalyxLedgerVerifyReport>;
    /// Records one permanently unverifiable raw-commitment cohort seal so
    /// verification of every later seal can resume. Appends an `Admin` Ledger
    /// entry; repairs and hides nothing.
    fn adjudicate_calyx_raw_commitment_seal(
        &self,
        ledger_seq: u64,
        expected_failure_sha256: &str,
        reason: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSealAdjudicationReceipt>;
    /// Reads and decodes one physical provenance-ledger entry by sequence.
    fn read_calyx_ledger_entry(&self, seq: u64) -> StorageResult<SynapseCalyxLedgerEntryReadback>;
    /// Re-derives a record's recorded provenance binding and bounds drift.
    fn reproduce_calyx_record(&self, cx_id: &str) -> StorageResult<SynapseCalyxReproduceReport>;
    /// Reads the authoritative source-row pointer from one physical Calyx Base row.
    fn read_calyx_base_source_pointer(
        &self,
        cx_id: &str,
    ) -> StorageResult<Option<synapse_calyx::SynapseCalyxBaseSourcePointer>>;
    /// Lawfully erases one record by content-addressed id via a ledger-stamped
    /// tombstone, then re-verifies the chain.
    fn erase_calyx_record(&self, cx_id: &str) -> StorageResult<SynapseCalyxErasureReport>;
    fn put_recurrence_subject_occurrence(
        &self,
        kind: RecurrenceSubjectKind,
        subject_id: &str,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<CalyxRecurrenceSubjectReport>;
    fn put_action_oracle_publication(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<ActionOraclePublicationReport>;
    fn oracle_predict_action(&self, query_cx_id: &str) -> StorageResult<Value>;
    fn oracle_reverse_action(&self, outcome: bool) -> StorageResult<Value>;
    fn oracle_complete_action(&self, cx_id: &str, free_slots: &[u16]) -> StorageResult<Value>;
    fn oracle_validate_action(&self) -> StorageResult<Value>;
    fn oracle_measure_readiness(&self) -> StorageResult<Value>;
    fn oracle_readiness(&self) -> StorageResult<Option<Value>>;
    fn append_autonomy_decision(
        &self,
        routine_id: &str,
        decision: &Value,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAutonomyDecisionReadback>;
    fn persist_recurrence_finding(
        &self,
        finding: &SynapseCalyxPersistedRecurrenceFinding,
    ) -> StorageResult<SynapseCalyxPersistedRecurrenceFinding>;
    fn persist_novelty_finding(
        &self,
        finding: &SynapseCalyxPersistedNoveltyFinding,
    ) -> StorageResult<SynapseCalyxPersistedNoveltyFinding>;
    fn persisted_novelty_findings(
        &self,
        after_ledger_seq: u64,
        max_rows: usize,
    ) -> StorageResult<Vec<SynapseCalyxPersistedNoveltyFinding>>;
    fn novelty_delivery_cursor(&self) -> StorageResult<u64>;
    fn persist_novelty_delivery_cursor(&self, ledger_seq: u64) -> StorageResult<u64>;
    fn persisted_region_findings(
        &self,
        after_observed_seq: u64,
        max_rows: usize,
    ) -> StorageResult<Vec<SynapseCalyxPersistedRegionFinding>>;
    fn region_delivery_cursor(&self) -> StorageResult<u64>;
    fn persist_region_delivery_cursor(&self, observed_seq: u64) -> StorageResult<u64>;
    fn read_recurrence_subject_series(
        &self,
        kind: RecurrenceSubjectKind,
        subject_id: &str,
    ) -> StorageResult<SynapseCalyxRecurrenceSeriesReadback>;
    fn list_temporal_panels(&self) -> StorageResult<Vec<VaultTemporalPanelRegistration>>;
    fn add_panel_lens(
        &self,
        panel_version: u32,
        operation_id: &str,
        slot_key: &str,
        lens_spec: calyx_registry::LensSpec,
        source_projection: synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection,
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxAddLensReadback>;
    fn set_panel_lens_state(
        &self,
        panel_version: u32,
        operation_id: &str,
        slot_id: calyx_core::SlotId,
        state: calyx_core::SlotState,
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxSetLensStateReadback>;
    fn read_panel_lifecycle(
        &self,
        panel_version: u32,
    ) -> StorageResult<Option<synapse_calyx::panel_lifecycle::SynapseCalyxPanelLifecycleState>>;
    fn publish_graph_position_snapshot(
        &self,
        kind: constellations::GraphPositionKind,
        source_seq: u64,
        created_at_ms: u64,
        transitions: &[(String, String, u64)],
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback>;
    fn publish_path_hierarchy_snapshot(
        &self,
        source_seq: u64,
        created_at_ms: u64,
        paths: &[String],
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback>;
    /// Retires every generation a dynamic panel owns other than the successor
    /// that just committed (#2062 ask 1).
    fn supersede_panel_generations(
        &self,
        panel_name: &str,
        successor: u32,
    ) -> StorageResult<synapse_calyx::PanelGenerationSupersession>;
    /// Reads the vault-global panel generation allocator's ownership authority.
    fn panel_generation_allocator(
        &self,
    ) -> StorageResult<synapse_calyx::PanelGenerationAllocatorReadback>;
    fn run_panel_backfill(
        &self,
        panel_version: u32,
        limit: usize,
        recover_in_flight: bool,
    ) -> StorageResult<PanelLifecycleBackfillReport>;
    fn temporal_rerank(
        &self,
        candidates: &[SynapseCalyxTemporalCandidate],
        query_time_secs: i64,
        tz_offset_secs: i32,
    ) -> StorageResult<SynapseCalyxTemporalRerankReadback>;
    /// Confirms exact-match-by-hash candidates against their authoritative
    /// source fields, dropping bucket collisions (#1899).
    fn confirm_exact_matches(
        &self,
        panel_version: u32,
        slot: u16,
        value: &str,
        cx_ids: &[String],
    ) -> StorageResult<Vec<constellations::ExactMatchConfirmation>>;
    #[allow(clippy::too_many_lines)]
    fn backfill_temporal_metadata(
        &self,
        source_cf: &str,
        source_keys: Option<&[Vec<u8>]>,
        pointwise_preflight: bool,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<constellations::TemporalMetadataBackfillReport>;
    /// Releases process-local anchor-lineage ownership at an explicit repair
    /// lifecycle boundary. `None` releases every completed source scope.
    fn release_temporal_backfill_lineage(&self, source_cf: Option<&str>) -> usize;
    fn put_timeline_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &TimelineRecord,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_episode_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &EpisodeRecord,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_episode_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, EpisodeRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>>;
    fn put_agent_event_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &AgentEventRecord,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_agent_event_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, AgentEventRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>>;
    fn put_agent_transcript_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &AgentTranscriptRecord,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_agent_transcript_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, AgentTranscriptRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>>;
    fn put_action_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_reflex_audit_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &StoredReflexAudit,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_reflex_lifecycle_grounded_publication(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &StoredReflexAudit,
    ) -> StorageResult<ReflexRegistrationPublicationReport>;
    fn put_reflex_lifecycle_grounded_batch_publication(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
        members: Vec<ReflexGroundedLifecycleMember>,
    ) -> StorageResult<ReflexLifecycleBatchPublicationReport>;
    fn put_process_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_sampled_observation_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &StoredObservation,
    ) -> StorageResult<Option<ConstellationPutReport>>;
    fn put_outcome_constellation(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_mcp_usage_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport>;
    fn put_mcp_usage_grounded_publication(
        &self,
        source_rows: Vec<RawRow>,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
        anchor: GroundingAnchor,
        ledger_payload: &Value,
    ) -> StorageResult<McpUsageGroundedPublicationReport>;
    fn put_grounding_anchor_for_source(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
        anchor: GroundingAnchor,
        ledger_payload: &Value,
    ) -> StorageResult<CalyxAnchorWriteReport>;
    fn put_grounding_anchors_for_sources(
        &self,
        sources: Vec<GroundingAnchorSource>,
        ledger_payload: &Value,
    ) -> StorageResult<CalyxAnchorBatchWriteReport>;
    fn calyx_anchor_scan_for_source(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
    ) -> StorageResult<CalyxAnchorScanReport>;
    fn run_pressure_check_once(
        &self,
        storage_path: &Path,
    ) -> StorageResult<pressure::PressureReport>;
    fn run_pressure_check_with_free_bytes_sample(
        &self,
        free_bytes: u64,
    ) -> StorageResult<pressure::PressureReport>;
    fn spawn_pressure_task(&self, storage_path: &Path) -> StorageResult<pressure::PressureTask>;
    fn scan_cf(&self, cf_name: &str) -> StorageResult<Vec<RawRow>>;
    fn scan_cf_prefix(&self, cf_name: &str, prefix: &[u8]) -> StorageResult<Vec<RawRow>>;
    fn scan_cf_prefix_from(
        &self,
        cf_name: &str,
        prefix: &[u8],
        start_key: &[u8],
    ) -> StorageResult<Vec<RawRow>>;
    fn scan_cf_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow>;
    fn scan_cf_physical_page(
        &self,
        cf_name: &str,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage>;
    fn scan_cf_range(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow>;
    fn scan_cf_fixed_width_range_page(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        after_key: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage>;
    fn pin_cf_physical_scan(
        &self,
        cf_name: &str,
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease>;
    fn pin_cf_fixed_width_range_scan(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease>;
    fn scan_cf_physical_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage>;
    fn scan_cf_fixed_width_range_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage>;
    fn release_coherent_scan(&self, lease: &mut CoherentScanLease) -> StorageResult<bool>;
    fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>>;
    fn compact_cf(&self, cf_name: &str) -> StorageResult<()>;
    fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()>;
    fn weave_panel_intelligence(
        &self,
        params: SynapseCalyxWeaveParams,
    ) -> StorageResult<SynapseCalyxWeaveReport>;
    fn publish_panel_input_snapshot(
        &self,
        panel_version: u32,
        chunk_rows: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPanelInputSnapshotReport>;
    fn prune_panel_input_changes(
        &self,
        panel_version: u32,
        through_seq: u64,
        mutation_through_seq: u64,
        max_rows: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPanelInputPruneReport>;
    fn commission_search_kernels(
        &self,
        params: &SynapseCalyxSearchCommissionParams,
    ) -> StorageResult<SynapseCalyxSearchCommissionReport>;
    fn abundance_report_intelligence(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> StorageResult<SynapseCalyxAbundanceReport>;
    fn assay_bits_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseCalyxBitsReport>;
    fn assay_sufficiency_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseCalyxSufficiencyReport>;
    /// Runs the ensemble capability card: per-lens marginal value, the PID
    /// triple, the A37 associational-diversity gate and a keep/park/retire
    /// verdict per lens (#1668 admission gate, wired for #1944 ask 1).
    fn assay_ensemble_card_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> StorageResult<SynapseCalyxEnsembleCardReport>;
    fn measure_causal_view_registry_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> StorageResult<SynapseCalyxCausalViewRegistryReadback>;
    fn read_causal_view_registry_intelligence(
        &self,
        scope: &SynapseCalyxCausalViewRegistryScope,
    ) -> StorageResult<Option<SynapseCalyxCausalViewRegistryReadback>>;
    fn assay_redundancy_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseCalyxRedundancyReport>;
    fn assay_synergy_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseSynergyReport>;
    fn temporal_causality_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxCausalityReport>;
    fn temporal_causal_map_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
        fdr_alpha: f32,
    ) -> StorageResult<SynapseCalyxCausalMapReport>;
    fn read_temporal_causal_map_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
        fdr_alpha: f32,
    ) -> StorageResult<SynapseCalyxCausalMapReport>;
    fn temporal_periodicity_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxPeriodicityReport>;
    fn temporal_drift_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxDriftReport>;
    fn temporal_hazard_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxHazardReport>;
    fn build_domain_kernel_intelligence(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> StorageResult<SynapseCalyxKernelReport>;
    fn kernel_answer_intelligence(
        &self,
        params: &SynapseCalyxKernelParams,
        query_cx_id: &str,
        max_hops: usize,
    ) -> StorageResult<SynapseCalyxKernelAnswerReport>;
    fn grounding_gap_intelligence(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> StorageResult<SynapseCalyxGroundingGapReport>;
    fn blind_spot_intelligence(
        &self,
        params: &SynapseCalyxBlindSpotParams,
    ) -> StorageResult<SynapseCalyxBlindSpotReport>;
    fn panel_drift_intelligence(
        &self,
        params: &SynapseCalyxPanelDriftParams,
    ) -> StorageResult<SynapseCalyxPanelDriftReport>;
    fn rebuild_domain_kernels_intelligence(
        &self,
        params: &SynapseCalyxKernelRebuildParams,
    ) -> StorageResult<SynapseCalyxKernelRebuildReport>;
    fn domain_kernel_health_intelligence(
        &self,
        panel_version: u32,
        content_slot: u16,
        anchor_kind: Option<&str>,
    ) -> StorageResult<SynapseCalyxKernelHealthReport>;
    fn guard_calibrate_intelligence(
        &self,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> StorageResult<SynapseCalyxGuardCalibrateReport>;
    fn guard_verify_intelligence(
        &self,
        params: &SynapseCalyxGuardVerifyParams,
    ) -> StorageResult<SynapseCalyxGuardVerifyReport>;
}

pub struct CalyxBackend {
    path: PathBuf,
    vault: Arc<CalyxVaultRuntime>,
    /// One GC authority per opened vault. Scheduled maintenance and explicit
    /// MCP GC calls share its exact reachability census and pressure state;
    /// constructing a second runner would duplicate a corpus-sized index.
    gc_runner: Arc<CalyxGcRunner>,
    pressure: Arc<pressure::PressureState>,
    anchor_carry_lineage: Mutex<AnchorCarryLineageCache>,
    storage_snapshots: Mutex<BTreeMap<u64, PinnedStorageSnapshot>>,
}

#[derive(Clone, Copy, Debug)]
struct PinnedStorageSnapshot {
    snapshot: Snapshot,
    opened_at_unix_ms: u64,
}

/// A process-local Aster MVCC lease opened at one committed sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalyxStorageSnapshotLease {
    pub lease_id: u64,
    pub snapshot_seq: u64,
    pub opened_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub max_age_ms: u64,
    pub active_lease_count: u64,
}

/// Metadata-only readback of one logical Synapse or native Calyx row through
/// an Aster snapshot.
///
/// Payload bytes stay behind their typed MCP owners. Length and SHA-256 prove
/// exact historical identity without turning this engine-level capability into
/// a raw-content exfiltration surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalyxStorageSnapshotReadback {
    pub lease_id: u64,
    pub snapshot_seq: u64,
    pub current_seq: u64,
    pub opened_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub cf_name: String,
    pub physical_present: bool,
    pub logical_present: bool,
    pub expired_at_snapshot: bool,
    pub written_at_unix_ms: Option<u64>,
    pub retention_expires_at_unix_ms: Option<u64>,
    pub payload_len_bytes: Option<u64>,
    pub payload_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalyxStorageSnapshotRelease {
    pub lease_id: u64,
    pub snapshot_seq: u64,
    pub current_seq: u64,
    pub released: bool,
    pub active_lease_count: u64,
}

type AnchorCarryLineageCache = BTreeMap<(String, Vec<u32>), Arc<BTreeMap<String, Vec<Anchor>>>>;

/// Grounded-anchor-lineage index builds, cache hits, and time spent building
/// (#2080 defect 3).
///
/// Published because the alternative is inference. The anchor-debt repair's
/// `ms_per_identity` advisory accused this read of "not amortizing" purely from
/// a wall-clock ratio, and that ratio could not distinguish a lineage index
/// rebuilt once per row from one rebuilt once per panel and divided by a
/// collapsing attempt count. These counters answer that question with a
/// measurement instead: one build per `(source_cf, superseded set)` per pass is
/// amortized; more than one build per attempted panel is not.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnchorCarryLineageCounters {
    pub builds: u64,
    pub hits: u64,
    pub build_ms: u64,
}

static ANCHOR_CARRY_LINEAGE_BUILDS: AtomicU64 = AtomicU64::new(0);
static ANCHOR_CARRY_LINEAGE_HITS: AtomicU64 = AtomicU64::new(0);
static ANCHOR_CARRY_LINEAGE_BUILD_MS: AtomicU64 = AtomicU64::new(0);

/// Process-lifetime lineage cache counters. Callers take a delta across a phase
/// rather than reading an absolute.
#[must_use]
pub fn anchor_carry_lineage_counters() -> AnchorCarryLineageCounters {
    AnchorCarryLineageCounters {
        builds: ANCHOR_CARRY_LINEAGE_BUILDS.load(Ordering::Relaxed),
        hits: ANCHOR_CARRY_LINEAGE_HITS.load(Ordering::Relaxed),
        build_ms: ANCHOR_CARRY_LINEAGE_BUILD_MS.load(Ordering::Relaxed),
    }
}

impl AnchorCarryLineageCounters {
    /// Work done between an earlier reading and this one.
    #[must_use]
    pub const fn since(self, before: Self) -> Self {
        Self {
            builds: self.builds.saturating_sub(before.builds),
            hits: self.hits.saturating_sub(before.hits),
            build_ms: self.build_ms.saturating_sub(before.build_ms),
        }
    }
}

impl CalyxBackend {
    #[expect(
        clippy::too_many_lines,
        reason = "one atomic lifecycle batch keeps guard preparation, constellation construction, commit, and physical readback in one auditable invariant"
    )]
    fn put_reflex_lifecycle_grounded_batch_publication_inner(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
        members: &[ReflexGroundedLifecycleMember],
    ) -> StorageResult<ReflexLifecycleBatchPublicationReport> {
        const OPERATION_CF: &str = "calyx_reflex_lifecycle_batch_publication";
        if members.is_empty() {
            return Err(calyx_write_failed_detail(
                OPERATION_CF,
                "REFLEX_LIFECYCLE_BATCH_EMPTY: remediation=do not call the durable batch boundary for an empty lifecycle transition".to_owned(),
            ));
        }
        validate_cross_cf_revision_guarded_put(&guards, &batches)?;
        let audit_row_count = batches
            .iter()
            .filter(|(cf_name, _rows)| cf_name == cf::CF_REFLEX_AUDIT)
            .map(|(_cf_name, rows)| rows.len())
            .sum::<usize>();
        if audit_row_count != members.len() {
            return Err(calyx_write_failed_detail(
                OPERATION_CF,
                format!(
                    "REFLEX_LIFECYCLE_BATCH_MEMBER_COUNT_MISMATCH: audit_rows={audit_row_count} members={}; remediation=rebuild the batch with exactly one grounded member per audit source row",
                    members.len()
                ),
            ));
        }
        for member in members {
            validate_reflex_registration_publication(
                &guards,
                &batches,
                &member.source_key,
                &member.raw_bytes,
                &member.record,
            )?;
        }
        let started = Instant::now();
        let member_count = members.len();
        let result = self.with_vault(
            OPERATION_CF,
            "atomically publish a revision-guarded reflex lifecycle batch",
            true,
            |vault| {
                let now_ms = calyx_clock_now_for_write(vault, OPERATION_CF)?;
                let mut physical_guards = Vec::with_capacity(guards.len());
                for guard in guards {
                    let collection_id = calyx_collection_id_for_cf_write(&guard.cf_name)?;
                    let physical_key =
                        encode_calyx_key_for_write(&guard.cf_name, collection_id, &guard.key)?;
                    physical_guards.push(SynapseCalyxRevisionGuard::new(
                        ColumnFamily::Kv,
                        physical_key,
                        guard.expected_revision_sha256,
                    ));
                }
                let mut physical_rows = Vec::new();
                for (cf_name, rows) in batches {
                    let collection_id = calyx_collection_id_for_cf_write(&cf_name)?;
                    for (key, value) in rows {
                        physical_rows.push(calyx_put_row(
                            &cf_name,
                            collection_id,
                            &key,
                            &value,
                            now_ms,
                        )?);
                    }
                }
                let expected_rows = physical_rows.clone();
                let mut grounded_members = Vec::with_capacity(members.len());
                let mut expected_anchors = Vec::with_capacity(members.len());
                let mut identities = Vec::with_capacity(members.len());
                for member in members {
                    let context = NativeConstellationContext {
                        vault_id: vault.vault_id_value(),
                        cx_id: vault.cx_id_for_input(
                            &member.raw_bytes,
                            SYN_REFLEX_PANEL_VERSION,
                        ),
                        created_at_ms: now_ms,
                        next_ledger_seq: vault.latest_seq().saturating_add(1),
                    };
                    let constellation = constellations::build_reflex_audit_constellation(
                        context,
                        &member.source_key,
                        &member.raw_bytes,
                        &member.record,
                    )?;
                    let lifecycle_state = reflex_lifecycle_state(&member.record)?;
                    let anchor = grounding_anchor_to_calyx(GroundingAnchor {
                        kind_label: "reflex_lifecycle_state".to_owned(),
                        value: GroundingAnchorValue::Enum(lifecycle_state.to_owned()),
                        source: "synapse-reflex-lifecycle".to_owned(),
                        observed_at_ms: member.record.ts_ns / 1_000_000,
                        confidence: 1.0,
                    })?;
                    expected_anchors.push((context.cx_id, anchor.clone()));
                    identities.push(serde_json::json!({
                        "reflex_id": member.record.reflex_id,
                        "audit_id": member.record.audit_id,
                        "state": lifecycle_state,
                        "source_key_sha256": constellations::sha256_hex(&member.source_key),
                        "source_value_sha256": constellations::sha256_hex(&member.raw_bytes),
                    }));
                    grounded_members.push((member.raw_bytes.clone(), constellation, anchor));
                }
                let ledger_payload = serde_json::to_vec(&serde_json::json!({
                    "schema": "synapse.reflex.lifecycle.batch.v1",
                    "members": identities,
                    "publication_row_count": physical_rows.len(),
                }))
                .map_err(|source| StorageError::EncodeJson {
                    type_name: "reflex_lifecycle_batch_grounding_ledger_payload",
                    source,
                })?;
                let write = vault
                    .put_guarded_grounded_observation_batch_with_source_rows(
                        physical_rows,
                        physical_guards,
                        grounded_members,
                        ledger_payload,
                        "synapse-reflex-lifecycle",
                    )
                    .map_err(|source| {
                        calyx_write_failed(
                            OPERATION_CF,
                            "commit atomic reflex lifecycle batch source/constellation/anchor rows",
                            &source,
                        )
                    })?;
                if write.source_row_count != expected_rows.len() {
                    return Err(calyx_write_failed_detail(
                        OPERATION_CF,
                        format!(
                            "REFLEX_LIFECYCLE_BATCH_COMMITTED_ROW_COUNT_MISMATCH: committed={} expected={}; remediation=preserve the vault and inspect atomic WAL sequence {}",
                            write.source_row_count,
                            expected_rows.len(),
                            write.committed_seq
                        ),
                    ));
                }
                let mut exact_match_count = 0_usize;
                for member in members {
                    let readback = verify_grounded_source_readback(
                        vault,
                        &expected_rows,
                        cf::CF_REFLEX_AUDIT,
                        &member.source_key,
                        &member.raw_bytes,
                        OPERATION_CF,
                        "reflex lifecycle batch",
                    )?;
                    exact_match_count = exact_match_count
                        .checked_add(readback.exact_match_count)
                        .ok_or_else(|| {
                            calyx_write_failed_detail(
                                OPERATION_CF,
                                "REFLEX_LIFECYCLE_BATCH_READBACK_COUNT_OVERFLOW: remediation=widen the exact-match counter".to_owned(),
                            )
                        })?;
                }
                for (cx_id, anchor) in expected_anchors {
                    verify_grounded_anchor_readback(
                        vault,
                        cx_id,
                        &anchor,
                        OPERATION_CF,
                        "reflex lifecycle batch",
                    )?;
                }
                Ok(ReflexLifecycleBatchPublicationReport {
                    member_count: u64::try_from(member_count).unwrap_or(u64::MAX),
                    source_row_count: u64::try_from(write.source_row_count).unwrap_or(u64::MAX),
                    source_readback_exact_match_count: u64::try_from(exact_match_count)
                        .unwrap_or(u64::MAX),
                    committed_seq: write.committed_seq,
                    cx_ids: write.cx_ids,
                    ledger_seq: write.ledger_seq,
                    ledger_hash: write.ledger_hash,
                })
            },
        );
        match result {
            Ok(report) => {
                tracing::info!(
                    code = "CALYX_REFLEX_LIFECYCLE_BATCH_ATOMIC_PUBLICATION_COMMITTED",
                    member_count = report.member_count,
                    source_row_count = report.source_row_count,
                    source_readback_exact_match_count = report.source_readback_exact_match_count,
                    committed_seq = report.committed_seq,
                    ledger_seq = report.ledger_seq,
                    duration_us = constellations::duration_us(started.elapsed()),
                    "reflex lifecycle batch committed atomically with physical readback"
                );
                Ok(report)
            }
            Err(error) => {
                tracing::error!(
                    code = "REFLEX_LIFECYCLE_BATCH_ATOMIC_PUBLICATION_FAILED",
                    member_count,
                    error_code = error.code(),
                    detail = %error,
                    "reflex lifecycle batch failed before scheduler publication"
                );
                Err(error)
            }
        }
    }
}

/// Shared lifecycle owner for the process-local Calyx vault.
///
/// Aster already provides its own fine-grained row/router locks and a durable
/// commit lock. Holding a second process-wide mutex across every storage
/// operation collapsed that concurrency and let a long GC pass block unrelated
/// reads and MCP response persistence. The lifecycle lock here is held only
/// long enough to clone the immutable vault handle; physical operations then
/// execute against Aster's authoritative synchronization.
struct CalyxVaultRuntime {
    vault: RwLock<Option<Arc<SynapseCalyxVault>>>,
}

impl CalyxVaultRuntime {
    fn new(vault: SynapseCalyxVault) -> Self {
        Self {
            vault: RwLock::new(Some(Arc::new(vault))),
        }
    }

    fn with_vault<T>(
        &self,
        cf_name: &str,
        operation: &'static str,
        write: bool,
        f: impl FnOnce(&SynapseCalyxVault) -> StorageResult<T>,
    ) -> StorageResult<T> {
        let vault = {
            let guard = self.vault.read().map_err(|poisoned| {
                calyx_operation_failed(
                    cf_name,
                    write,
                    format!("{operation}: Calyx vault lifecycle lock poisoned: {poisoned}"),
                )
            })?;
            guard.as_ref().cloned().ok_or_else(|| {
                calyx_operation_failed(
                    cf_name,
                    write,
                    format!("{operation}: Calyx vault handle has already been closed"),
                )
            })?
        };
        f(&vault)
    }

    fn status(&self) -> StorageResult<SynapseCalyxVaultStatus> {
        self.with_vault(
            "<calyx-vault>",
            "read live Calyx vault status",
            false,
            |vault| Ok(vault.status()),
        )
    }

    fn oracle_predict_action(&self, query_cx_id: &str) -> StorageResult<Value> {
        let query_cx_id = query_cx_id.parse::<calyx_core::CxId>().map_err(|error| {
            let source = SynapseCalyxError::new(
                "SYNAPSE_CALYX_CX_ID_INVALID",
                format!("invalid causal prediction query_cx_id: {error}"),
                "supply the exact 32-hex-character id of a persisted current action constellation",
            );
            calyx_write_failed("calyx_oracle", "parse causal prediction query", &source)
        })?;
        self.with_vault(
            "calyx_oracle",
            "predict terminal action outcome from typed pre-trigger causes",
            true,
            |vault| {
                let _ = current_action_causal_registry(vault)?;
                vault
                    .predict_typed_action_outcome(query_cx_id)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_oracle",
                            "predict action outcome from typed pre-trigger causes",
                            &source,
                        )
                    })
                    .and_then(|prediction| {
                        serde_json::to_value(prediction).map_err(|error| {
                            calyx_write_failed_detail(
                                "calyx_oracle",
                                format!("encode typed causal prediction: {error}"),
                            )
                        })
                    })
            },
        )
    }

    fn oracle_reverse_action(&self, outcome: bool) -> StorageResult<Value> {
        self.with_vault(
            "calyx_oracle",
            "reverse terminal action outcome",
            true,
            |vault| {
                vault
                    .oracle_reverse_action(&AnchorValue::Bool(outcome), "synapse.action")
                    .map_err(|source| {
                        calyx_write_failed("calyx_oracle", "reverse action outcome", &source)
                    })
            },
        )
    }

    fn oracle_complete_action(&self, cx_id: &str, free_slots: &[u16]) -> StorageResult<Value> {
        let cx_id = cx_id.parse::<calyx_core::CxId>().map_err(|error| {
            let source = SynapseCalyxError::new(
                "SYNAPSE_CALYX_CX_ID_INVALID",
                format!("invalid completion cx_id: {error}"),
                "supply the exact 32-hex-character constellation id read from the action panel",
            );
            calyx_write_failed("calyx_oracle", "parse completion constellation id", &source)
        })?;
        self.with_vault("calyx_oracle", "complete action constellation", true, |vault| {
            let created_at_ms = calyx_clock_now_for_write(vault, "calyx_oracle")?;
            let mut panel = syn_queryable_panel_contract(SYN_ACTION_PANEL_VERSION, created_at_ms)?
                .ok_or_else(|| calyx_write_failed_detail("calyx_oracle", "syn-action panel contract is absent"))?
                .panel;
            if free_slots.is_empty() {
                let source = SynapseCalyxError::new(
                    "SYNAPSE_CALYX_ORACLE_COMPLETION_FREE_EMPTY",
                    "Oracle completion requires at least one explicitly free slot",
                    "supply one or more slot ids from the declared action panel contract",
                );
                return Err(calyx_write_failed("calyx_oracle", "validate completion slots", &source));
            }
            if let Some(slot_id) = free_slots.iter().find(|slot_id| {
                !panel.slots.iter().any(|slot| slot.slot_id.get() == **slot_id)
            }) {
                let source = SynapseCalyxError::new(
                    "SYNAPSE_CALYX_ORACLE_COMPLETION_SLOT_UNKNOWN",
                    format!("slot {slot_id} is not declared by action panel {SYN_ACTION_PANEL_VERSION}"),
                    "read the active action panel contract and supply only one of its slot ids",
                );
                return Err(calyx_write_failed("calyx_oracle", "validate completion slots", &source));
            }
            let target = vault.hydrate_constellation_latest(cx_id).map_err(|source| {
                calyx_write_failed("calyx_oracle", "read completion target", &source)
            })?;
            if target.panel_version != SYN_ACTION_PANEL_VERSION {
                let source = SynapseCalyxError::new(
                    "SYNAPSE_CALYX_ORACLE_COMPLETION_PANEL_MISMATCH",
                    format!("constellation {cx_id} belongs to panel {}, not {SYN_ACTION_PANEL_VERSION}", target.panel_version),
                    "supply a constellation id read from the active action panel",
                );
                return Err(calyx_write_failed("calyx_oracle", "validate completion target", &source));
            }
            let readiness = vault.read_action_readiness().map_err(|source| {
                calyx_read_failed(
                    "calyx_oracle",
                    "read authenticated action readiness before completion",
                    &source,
                )
            })?.ok_or_else(|| {
                calyx_write_failed_detail(
                    "calyx_oracle",
                    "action Oracle completion requires an authenticated current readiness row",
                )
            })?;
            synapse_calyx::ensure_action_readiness_serving_admitted(&readiness).map_err(|source| {
                calyx_write_failed(
                    "calyx_oracle",
                    "admit authenticated action readiness before completion",
                    &source,
                )
            })?;
            let unsupported_free_slots = free_slots
                .iter()
                .copied()
                .filter(|slot| !readiness.sufficiency_slots.contains(slot))
                .collect::<Vec<_>>();
            if !unsupported_free_slots.is_empty() {
                let source = SynapseCalyxError::new(
                    "SYNAPSE_CALYX_ORACLE_COMPLETION_SLOT_UNMEASURED",
                    format!(
                        "completion free slots {unsupported_free_slots:?} are outside authenticated sufficiency roster {:?}",
                        readiness.sufficiency_slots
                    ),
                    "request only an estimator-backed slot named by oracle_readiness; physically valid constants remain serving causes but have no Lens Assay row to authorize completion",
                );
                return Err(calyx_write_failed(
                    "calyx_oracle",
                    "validate completion sufficiency roster",
                    &source,
                ));
            }
            panel
                .slots
                .retain(|slot| readiness.sufficiency_slots.contains(&slot.slot_id.get()));
            let free_slots = free_slots
                .iter()
                .copied()
                .map(calyx_core::SlotId::new)
                .collect::<std::collections::BTreeSet<_>>();
            vault
                .oracle_complete(cx_id, &panel, "synapse.action", &free_slots)
                .map_err(|source| calyx_write_failed("calyx_oracle", "complete action constellation", &source))
        })
    }

    fn oracle_measure_readiness(&self) -> StorageResult<Value> {
        self.with_vault("calyx_oracle", "measure action readiness", true, |vault| {
            let created_at_ms = calyx_clock_now_for_write(vault, "calyx_oracle")?;
            let mut panel = syn_queryable_panel_contract(SYN_ACTION_PANEL_VERSION, created_at_ms)?
                .ok_or_else(|| {
                    calyx_write_failed_detail("calyx_oracle", "syn-action panel contract is absent")
                })?
                .panel;
            let mut assay = synapse_calyx::SynapseCalyxAssayParams::new(
                SYN_ACTION_PANEL_VERSION,
                "reward".to_owned(),
            )
            .with_corpus_shard("synapse.action".to_owned())
            .with_lens_names(crate::constellations::syn_slot_lens_names())
            .with_required_record_slots([synapse_calyx::SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT]);
            let predictor_slots = ACTION_CAUSAL_PREDICTOR_SLOTS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            assay.excluded_slots.extend(
                panel
                    .slots
                    .iter()
                    .map(|slot| slot.slot_id.get())
                    .filter(|slot| !predictor_slots.contains(slot)),
            );
            panel
                .slots
                .retain(|slot| predictor_slots.contains(&slot.slot_id.get()));
            let snapshot = vault
                .measure_action_readiness_from_assay(&panel, &assay)
                .map_err(|source| {
                    calyx_write_failed("calyx_oracle", "measure action readiness", &source)
                })?;
            serde_json::to_value(snapshot).map_err(|error| {
                calyx_write_failed_detail(
                    "calyx_oracle",
                    format!("encode readiness snapshot: {error}"),
                )
            })
        })
    }

    fn oracle_validate_action(&self) -> StorageResult<Value> {
        self.with_vault(
            "calyx_oracle",
            "validate action readiness evidence",
            true,
            |vault| {
                let _ = current_action_causal_registry(vault)?;
                let evidence = vault
                    .validate_action_readiness(SYN_ACTION_PANEL_VERSION)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_oracle",
                            "validate action readiness evidence",
                            &source,
                        )
                    })?;
                serde_json::to_value(evidence).map_err(|error| {
                    calyx_write_failed_detail(
                        "calyx_oracle",
                        format!("encode action validation evidence: {error}"),
                    )
                })
            },
        )
    }

    fn oracle_readiness(&self) -> StorageResult<Option<Value>> {
        self.with_vault("calyx_oracle", "read action readiness", false, |vault| {
            vault
                .read_action_readiness()
                .map_err(|source| {
                    calyx_read_failed("calyx_oracle", "read action readiness", &source)
                })?
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| {
                    calyx_operation_failed(
                        "calyx_oracle",
                        false,
                        format!("encode readiness snapshot: {error}"),
                    )
                })
        })
    }

    fn append_autonomy_decision(
        &self,
        routine_id: &str,
        decision: &Value,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAutonomyDecisionReadback> {
        self.with_vault(
            "calyx_ledger",
            "append autonomy decision policy ledger",
            true,
            |vault| {
                vault
                    .append_autonomy_decision(routine_id, decision)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_ledger",
                            "append autonomy decision policy ledger",
                            &source,
                        )
                    })
            },
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "source publication, action occurrence, constellation, and independent readback are one atomic evidence transaction"
    )]
    fn put_action_oracle_publication_inner(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<ActionOraclePublicationReport> {
        if context.len() > calyx_aster::recurrence::MAX_CONTEXT_BYTES {
            return Err(calyx_write_failed_detail(
                "calyx_action_oracle_publication",
                format!(
                    "Oracle context is {} bytes; maximum is {}",
                    context.len(),
                    calyx_aster::recurrence::MAX_CONTEXT_BYTES
                ),
            ));
        }
        self.with_vault(
            "calyx_action_oracle_publication",
            "atomically publish terminal action and Oracle occurrence",
            true,
            |vault| {
                let action = record
                    .get("tool")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        calyx_write_failed_detail(
                            "calyx_action_oracle_publication",
                            "terminal action row requires a non-empty tool identity",
                        )
                    })?;
                let subject_id = validate_recurrence_subject_id(action)?;
                let subject_input = constellations::recurrence_subject_input_bytes(
                    RecurrenceSubjectKind::Action,
                    &subject_id,
                );
                let subject_cx_id =
                    vault.cx_id_for_input(&subject_input, SYN_RECURRENCE_SUBJECT_PANEL_VERSION);
                let created_at_ms = calyx_clock_now_for_write(vault, cf::CF_ACTION_LOG)?;
                let subject = constellations::build_recurrence_subject_constellation(
                    NativeConstellationContext {
                        vault_id: vault.vault_id_value(),
                        cx_id: subject_cx_id,
                        created_at_ms,
                        next_ledger_seq: vault.latest_seq().saturating_add(1),
                    },
                    RecurrenceSubjectKind::Action,
                    &subject_id,
                    &subject_input,
                )?;
                vault
                    .put_observation_constellation(subject)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_action_oracle_publication",
                            "ensure stable action recurrence subject",
                            &source,
                        )
                    })?;

                let action_cx_id = vault.cx_id_for_input(raw_bytes, SYN_ACTION_PANEL_VERSION);
                let action_constellation = constellations::build_action_constellation(
                    NativeConstellationContext {
                        vault_id: vault.vault_id_value(),
                        cx_id: action_cx_id,
                        created_at_ms,
                        next_ledger_seq: vault.latest_seq().saturating_add(1),
                    },
                    source_key,
                    raw_bytes,
                    record,
                )?;
                if action_constellation.anchors.len() != 1 {
                    return Err(calyx_write_failed_detail(
                        "calyx_action_oracle_publication",
                        format!(
                            "terminal action source must derive exactly one outcome anchor; derived {} for source_key_hex={}",
                            action_constellation.anchors.len(),
                            constellations::hex_encode(source_key)
                        ),
                    ));
                }
                let event_time_secs =
                    i64::try_from(event_time_ns / 1_000_000_000).map_err(|error| {
                        calyx_write_failed_detail(
                            "calyx_action_oracle_publication",
                            format!("event time does not fit EpochSecs: {error}"),
                        )
                    })?;
                let observed_at_secs = i64::try_from(created_at_ms / 1_000).map_err(|error| {
                    calyx_write_failed_detail(
                        "calyx_action_oracle_publication",
                        format!("Calyx clock does not fit EpochSecs: {error}"),
                    )
                })?;
                let mut identity_hasher = Sha256::new();
                identity_hasher.update(b"synapse-recurrence-occurrence-v1");
                identity_hasher.update([0]);
                identity_hasher.update(RecurrenceSubjectKind::Action.as_str().as_bytes());
                identity_hasher.update([0]);
                identity_hasher.update(subject_id.as_bytes());
                identity_hasher.update([0]);
                identity_hasher.update(occurrence_identity);
                let occurrence_identity_sha256: [u8; 32] = identity_hasher.finalize().into();
                let collection_id = calyx_collection_id_for_cf_write(cf::CF_ACTION_LOG)?;
                let source_row = calyx_put_row(
                    cf::CF_ACTION_LOG,
                    collection_id,
                    source_key,
                    raw_bytes,
                    created_at_ms,
                )?;
                let readback: SynapseCalyxAtomicConstellationRecurrenceReadback = vault
                    .append_recurrence_occurrence_with_constellation_rows(
                        subject_cx_id,
                        event_time_secs,
                        observed_at_secs,
                        context.to_vec(),
                        occurrence_identity_sha256,
                        action_constellation,
                        vec![source_row],
                        context.to_vec(),
                    )
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_action_oracle_publication",
                            "commit terminal action source/constellation/recurrence rows",
                            &source,
                        )
                    })?;
                Ok(ActionOraclePublicationReport {
                    subject_cx_id: readback.recurrence_cx_id,
                    constellation_cx_id: readback.constellation_cx_id,
                    occurrence_id: readback.occurrence_id,
                    committed_seq: readback.committed_seq,
                    latest_seq: readback.latest_seq,
                    source_row_count: readback.source_row_count,
                })
            },
        )
    }

    fn pin_cf_physical_scan(
        &self,
        cf_name: &str,
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease> {
        calyx_collection_id_for_cf_read(cf_name)?;
        self.with_vault(
            cf_name,
            "pin coherent physical Calyx scan",
            false,
            |vault| {
                pin_coherent_scan(
                    vault,
                    cf_name,
                    CoherentScanScope::PhysicalColumnFamily,
                    max_age_ms,
                )
            },
        )
    }

    fn pin_cf_fixed_width_range_scan(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease> {
        calyx_collection_id_for_cf_read(cf_name)?;
        let key_len = fixed_width_user_key_len(cf_name).unwrap_or(start_key.len());
        validate_fixed_width_page_request(cf_name, start_key, end_key, None, key_len, 1)?;
        self.with_vault(
            cf_name,
            "pin coherent fixed-width Calyx range scan",
            false,
            |vault| {
                pin_coherent_scan(
                    vault,
                    cf_name,
                    CoherentScanScope::FixedWidthRange {
                        start_key: start_key.to_vec(),
                        end_key: end_key.to_vec(),
                    },
                    max_age_ms,
                )
            },
        )
    }

    fn scan_cf_physical_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage> {
        ensure_coherent_scan_ready(lease, "physical_column_family")?;
        if !matches!(lease.scope, CoherentScanScope::PhysicalColumnFamily) {
            return Err(coherent_scan_contract_error(
                lease,
                "COHERENT_SCAN_SCOPE_MISMATCH: a fixed-width range lease cannot scan the whole column family",
            ));
        }
        let cf_name = lease.cf_name.clone();
        let after = lease.next_after.clone();
        let page = self.with_vault(
            &cf_name,
            "continue coherent physical Calyx scan",
            false,
            |vault| {
                read_physical_page_from_vault_snapshot(
                    vault,
                    &cf_name,
                    lease.snapshot,
                    after.as_deref(),
                    max_rows,
                    lease.read_at_unix_ms,
                )
            },
        )?;
        advance_coherent_scan(lease, page.resume_after_physical.as_deref(), page.more)?;
        Ok(page)
    }

    fn scan_cf_fixed_width_range_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage> {
        ensure_coherent_scan_ready(lease, "fixed_width_range")?;
        let (start_key, end_key) = match &lease.scope {
            CoherentScanScope::FixedWidthRange { start_key, end_key } => {
                (start_key.clone(), end_key.clone())
            }
            CoherentScanScope::PhysicalColumnFamily => {
                return Err(coherent_scan_contract_error(
                    lease,
                    "COHERENT_SCAN_SCOPE_MISMATCH: a whole-column-family lease cannot scan a fixed-width range",
                ));
            }
        };
        let cf_name = lease.cf_name.clone();
        let after = lease.next_after.clone();
        let key_len = fixed_width_user_key_len(&cf_name).unwrap_or(start_key.len());
        let page = self.with_vault(
            &cf_name,
            "continue coherent fixed-width Calyx range scan",
            false,
            |vault| {
                read_fixed_width_page_from_vault_range_snapshot(
                    vault,
                    &cf_name,
                    lease.snapshot,
                    &start_key,
                    &end_key,
                    after.as_deref(),
                    key_len,
                    max_rows,
                    lease.read_at_unix_ms,
                )
            },
        )?;
        advance_coherent_scan(lease, page.resume_after.as_deref(), page.more)?;
        Ok(page)
    }

    fn release_coherent_scan(&self, lease: &mut CoherentScanLease) -> StorageResult<bool> {
        if lease.released {
            return Err(coherent_scan_contract_error(
                lease,
                "COHERENT_SCAN_ALREADY_RELEASED: a snapshot lease can be released exactly once",
            ));
        }
        let cf_name = lease.cf_name.clone();
        let released =
            self.with_vault(&cf_name, "release coherent Calyx scan", false, |vault| {
                Ok(vault.release_reader(lease.lease_id))
            })?;
        lease.released = true;
        tracing::debug!(
            code = "STORAGE_COHERENT_SCAN_RELEASED",
            lease_id = lease.lease_id,
            snapshot_seq = lease.snapshot_seq,
            cf_name = %lease.cf_name,
            completed = lease.completed,
            released_live_lease = released,
            "released bounded coherent scan lease"
        );
        Ok(released)
    }

    fn close(
        &self,
        reason: &'static str,
        terminal_process: bool,
    ) -> StorageResult<SynapseCalyxVaultCloseReadback> {
        let vault = {
            let mut slot = self.vault.write().map_err(|poisoned| {
                calyx_write_failed_detail(
                    "<calyx-vault>",
                    format!("close Calyx vault: lifecycle lock poisoned: {poisoned}"),
                )
            })?;
            let vault = slot.take().ok_or_else(|| {
                calyx_write_failed_detail(
                    "<calyx-vault>",
                    "close Calyx vault: handle has already been closed".to_owned(),
                )
            })?;
            match Arc::try_unwrap(vault) {
                Ok(vault) => {
                    drop(slot);
                    vault
                }
                Err(vault) => {
                    let active_operations = Arc::strong_count(&vault).saturating_sub(1);
                    *slot = Some(vault);
                    drop(slot);
                    return Err(calyx_write_failed_detail(
                        "<calyx-vault>",
                        format!(
                            "close Calyx vault refused while {active_operations} physical operation handle(s) remain active"
                        ),
                    ));
                }
            }
        };
        let close = if terminal_process {
            vault.close_for_process_exit(reason)
        } else {
            vault.close(reason)
        };
        close.map_err(|source| {
            calyx_write_failed("<calyx-vault>", "flush and close live Calyx vault", &source)
        })
    }
}

impl Drop for CalyxVaultRuntime {
    fn drop(&mut self) {
        let slot = match self.vault.get_mut() {
            Ok(slot) => slot,
            Err(poisoned) => {
                tracing::error!(
                    code = "STORAGE_CALYX_DROP_LOCK_POISONED",
                    error = %poisoned,
                    "Calyx storage lifecycle lock poisoned during drop; attempting close anyway"
                );
                poisoned.into_inner()
            }
        };
        let Some(vault) = slot.take() else {
            return;
        };
        let vault = match Arc::try_unwrap(vault) {
            Ok(vault) => vault,
            Err(vault) => {
                tracing::error!(
                    code = "STORAGE_CALYX_DROP_ACTIVE_OPERATION",
                    strong_count = Arc::strong_count(&vault),
                    "Calyx storage runtime reached final drop with active vault operations; deterministic close refused"
                );
                return;
            }
        };
        if let Err(error) = vault.close("synapse_storage_calyx_runtime_drop") {
            tracing::error!(
                code = error.code,
                error = %error,
                "Calyx storage backend close failed during drop"
            );
        }
    }
}

impl CalyxBackend {
    fn anchor_carry_lineage(
        &self,
        source_cf: &str,
        reset_for_new_sweep: bool,
    ) -> StorageResult<Arc<BTreeMap<String, Vec<Anchor>>>> {
        let superseded = constellations::superseded_panel_versions_for_source_cf(source_cf)?;
        if superseded.is_empty() {
            self.release_anchor_carry_lineage(Some(source_cf));
            return Ok(Arc::new(BTreeMap::new()));
        }
        let cache_key = (source_cf.to_owned(), superseded.to_vec());
        if reset_for_new_sweep {
            let mut guard = self
                .anchor_carry_lineage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&cache_key);
            drop(guard);
        } else {
            let guard = self
                .anchor_carry_lineage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(by_source_key) = guard.get(&cache_key) {
                ANCHOR_CARRY_LINEAGE_HITS.fetch_add(1, Ordering::Relaxed);
                return Ok(Arc::clone(by_source_key));
            }
        }

        let build_started = Instant::now();
        let by_source_key = self.with_vault(
            "calyx_anchor_carry_lineage",
            "build grounded anchor lineage from superseded Base rows",
            false,
            |vault| {
                vault
                    .grounded_anchor_lineage_by_source_key(
                        backfill_physical_source_cf(source_cf),
                        superseded,
                    )
                    .map_err(|error| {
                        calyx_read_failed(
                            "calyx_anchor_carry_lineage",
                            "build grounded anchor lineage from superseded Base rows",
                            &error,
                        )
                    })
            },
        )?;
        ANCHOR_CARRY_LINEAGE_BUILDS.fetch_add(1, Ordering::Relaxed);
        ANCHOR_CARRY_LINEAGE_BUILD_MS.fetch_add(
            u64::try_from(build_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let by_source_key = Arc::new(by_source_key);
        let mut guard = self
            .anchor_carry_lineage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(cache_key, Arc::clone(&by_source_key));
        drop(guard);
        Ok(by_source_key)
    }

    fn release_anchor_carry_lineage(&self, source_cf: Option<&str>) -> usize {
        let mut guard = self
            .anchor_carry_lineage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entries_before = guard.len();
        let rows_released = guard
            .iter()
            .filter(|((cached_source_cf, _versions), _lineage)| {
                source_cf.is_none_or(|source_cf| cached_source_cf == source_cf)
            })
            .map(|(_key, lineage)| lineage.len())
            .sum::<usize>();
        guard.retain(|(cached_source_cf, _versions), _lineage| {
            source_cf.is_some_and(|source_cf| cached_source_cf != source_cf)
        });
        let entries_released = entries_before.saturating_sub(guard.len());
        if entries_released != 0 {
            tracing::info!(
                code = "CALYX_ANCHOR_CARRY_LINEAGE_RELEASED",
                source_cf = source_cf.unwrap_or("all"),
                entries_released,
                rows_released,
                entries_remaining = guard.len(),
                "released completed temporal-backfill lineage ownership"
            );
        }
        entries_released
    }

    pub fn open(path: &Path, schema_version: u32) -> StorageResult<Self> {
        // Non-daemon callers retain the documented environment configuration
        // path. The daemon resolves CLI/environment precedence once and calls
        // `open_with_resolved_config` instead of using process environment as
        // an implicit cross-layer message bus.
        let config = SynapseCalyxConfig::from_optional_vault_dir_and_config_path(
            Some(path.to_path_buf()),
            std::env::var_os("SYNAPSE_CALYX_CONFIG").map(std::path::PathBuf::from),
        )
        .map_err(|source| calyx_open_failed(path, &source))?;
        Self::open_with_resolved_config(path, schema_version, config)
    }

    /// Opens the authoritative Calyx storage vault with configuration already
    /// resolved by the application boundary.
    ///
    /// # Errors
    ///
    /// Returns a structured open error when the resolved configuration names
    /// a different vault or when Calyx cannot open the exact requested path.
    pub fn open_with_resolved_config(
        path: &Path,
        schema_version: u32,
        config: SynapseCalyxConfig,
    ) -> StorageResult<Self> {
        let requested_vault = std::path::absolute(path).map_err(|error| StorageError::OpenFailed {
            path: path.to_path_buf(),
            detail: format!(
                "SYNAPSE_CALYX_STORAGE_PATH_RESOLUTION_FAILED: resolve authoritative storage vault path: {error}"
            ),
        })?;
        let configured_vault =
            std::path::absolute(&config.vault_dir).map_err(|error| StorageError::OpenFailed {
                path: path.to_path_buf(),
                detail: format!(
                    "SYNAPSE_CALYX_CONFIG_VAULT_PATH_RESOLUTION_FAILED: resolve configured Calyx vault {}: {error}",
                    config.vault_dir.display()
                ),
            })?;
        if configured_vault != requested_vault {
            return Err(StorageError::OpenFailed {
                path: path.to_path_buf(),
                detail: format!(
                    "SYNAPSE_CALYX_RESOLVED_CONFIG_VAULT_MISMATCH: resolved configuration vault {} differs from authoritative storage vault {}; resolve CLI/environment precedence once for the storage DB path and pass that exact configuration",
                    configured_vault.display(),
                    requested_vault.display()
                ),
            });
        }
        tracing::info!(
            code = "STORAGE_CALYX_RESOLVED_CONFIG_APPLIED",
            storage_path = %requested_vault.display(),
            math_backend = config.tuning.math_backend.as_str(),
            vram_budget_bytes = config.tuning.vram_budget_bytes,
            "applying one explicitly resolved Calyx configuration at the authoritative storage open"
        );
        // The durable router/SST set is the corpus Source of Truth. Rebuilding
        // every key and value into VersionedCfStore made daemon memory scale
        // with lifetime corpus size (21 GiB on the production vault) before a
        // single request ran. Latest-readback keeps only the WAL/live overlay
        // resident and serves the checkpointed corpus from immutable SSTs.
        // Live snapshot coherence is retained by Calyx's baseline+delta MVCC:
        // before the first post-open change to a key, it records that key's
        // router value (or absence) at the recovery floor, then journals later
        // versions only for changed keys. This is the normal writable mode,
        // not a retry or degraded fallback; failure to establish it fails the
        // storage open with the exact Calyx error and no full-restore escape.
        let vault = SynapseCalyxVault::open_latest_readback(config)
            .map_err(|source| calyx_open_failed(path, &source))?;
        prepare_calyx_open_fanout(&vault, path, "before_schema_and_migration")?;
        verify_calyx_schema_version(&vault, path, schema_version)?;
        ensure_calyx_ordered_key_migration(&vault, path)?;
        ensure_builtin_panel_generation_reservations(&vault)?;
        ensure_builtin_temporal_panel_registrations(&vault)?;
        ensure_active_panel_published(&vault)?;
        // A bulk migration can checkpoint many commit sequences at once. It
        // must not publish a backend whose newly materialized physical files
        // already exceed the same source ceiling the page readers enforce.
        prepare_calyx_open_fanout(&vault, path, "after_ordered_key_migration")?;
        let vault = Arc::new(CalyxVaultRuntime::new(vault));
        let gc_runner = Arc::new(CalyxGcRunner::new(Arc::clone(&vault)));
        Ok(Self {
            path: path.to_path_buf(),
            vault,
            gc_runner,
            pressure: Arc::new(pressure::PressureState::default()),
            anchor_carry_lineage: Mutex::new(BTreeMap::new()),
            storage_snapshots: Mutex::new(BTreeMap::new()),
        })
    }

    fn with_vault<T>(
        &self,
        cf_name: &str,
        operation: &'static str,
        write: bool,
        f: impl FnOnce(&SynapseCalyxVault) -> StorageResult<T>,
    ) -> StorageResult<T> {
        self.vault.with_vault(cf_name, operation, write, f)
    }

    fn lock_storage_snapshots(
        &self,
    ) -> StorageResult<std::sync::MutexGuard<'_, BTreeMap<u64, PinnedStorageSnapshot>>> {
        self.storage_snapshots.lock().map_err(|_| StorageError::ReadFailed {
            cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
            detail: "SYNAPSE_CALYX_SNAPSHOT_LEASE_TABLE_POISONED: the process-local MVCC lease table lock is poisoned; preserve daemon logs and restart the repo-built daemon before opening another historical reader".to_owned(),
        })
    }

    fn prune_expired_storage_snapshots(
        &self,
        vault: &SynapseCalyxVault,
        now_ms: u64,
    ) -> StorageResult<()> {
        let expired = {
            let mut snapshots = self.lock_storage_snapshots()?;
            let expired = snapshots
                .iter()
                .filter(|(_lease_id, pinned)| pinned.snapshot.lease().expires_at() <= now_ms)
                .map(|(lease_id, _pinned)| *lease_id)
                .collect::<Vec<_>>();
            for lease_id in &expired {
                snapshots.remove(lease_id);
            }
            expired
        };
        for lease_id in expired {
            if !vault.release_reader(lease_id) {
                tracing::debug!(
                    code = "SYNAPSE_CALYX_SNAPSHOT_LEASE_ALREADY_EXPIRED",
                    lease_id,
                    "expired public snapshot lease was already absent from Aster's live reader registry"
                );
            }
        }
        Ok(())
    }

    fn commit_rows(&self, cf_name: &str, rows: Vec<SynapseCalyxCfWrite>) -> StorageResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.with_vault(cf_name, "commit Calyx KV rows", true, |vault| {
            commit_calyx_rows_to_vault(vault, cf_name, rows)
        })
    }

    fn read_all_rows(&self, cf_name: &str) -> StorageResult<Vec<RawRow>> {
        self.with_vault(cf_name, "scan Calyx KV namespace", false, |vault| {
            read_all_rows_from_vault(vault, cf_name)
        })
    }
}

fn ensure_builtin_temporal_panel_registrations(vault: &SynapseCalyxVault) -> StorageResult<()> {
    let registered_at_unix_ms = calyx_clock_now_for_write(vault, "calyx_registry")?;
    // Recurrence series belong to stable app/routine subject CxIds, not event
    // constellations. Claiming recurrence evidence on these panel sidecars
    // would make provenance dishonest.
    let policy = TemporalPolicy {
        recurrence_boost: None,
        ..Default::default()
    };
    let expected = [
        (SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION),
        (SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION),
        (SYN_AGENT_EVENT_PANEL_NAME, SYN_AGENT_EVENT_PANEL_VERSION),
        (
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        ),
    ];
    for (panel_name, panel_version) in expected {
        let registration = VaultTemporalPanelRegistration::new(
            panel_version,
            panel_name,
            policy,
            registered_at_unix_ms,
        )
        .map_err(|source| {
            let source = SynapseCalyxError::from_calyx(
                "construct native Calyx temporal panel registration",
                &source,
            );
            calyx_write_failed(
                "calyx_registry",
                &format!(
                    "construct temporal panel registration {panel_name} generation {panel_version}"
                ),
                &source,
            )
        })?;
        let write = vault
            .register_temporal_panel(&registration)
            .map_err(|source| {
                calyx_write_failed(
                    "calyx_registry",
                    &format!("register temporal panel {panel_name} generation {panel_version}"),
                    &source,
                )
            })?;
        tracing::info!(
            code = "STORAGE_CALYX_TEMPORAL_PANEL_REGISTERED",
            panel_name,
            panel_version,
            disposition = ?write.disposition,
            committed_seq = write.committed_seq,
            "native Calyx temporal panel registration has exact Registry CF readback"
        );
    }
    let readback = vault.list_temporal_panels().map_err(|source| {
        calyx_write_failed(
            "calyx_registry",
            "read back native Calyx temporal panel catalog",
            &source,
        )
    })?;
    for (panel_name, panel_version) in expected {
        if !readback.iter().any(|registration| {
            registration.template.name == panel_name
                && registration.source_panel_version == panel_version
                && registration.policy == policy
        }) {
            return Err(StorageError::WriteFailed {
                cf_name: "calyx_registry".to_owned(),
                detail: format!(
                    "STORAGE_CALYX_TEMPORAL_PANEL_READBACK_MISSING: panel={panel_name} generation={panel_version}; remediation=inspect native Registry CF and refuse temporal queries until the exact contract exists"
                ),
            });
        }
    }
    Ok(())
}

/// Publishes an active durable `Panel` snapshot into the Calyx manifest at boot
/// so `storage/search_rebuild` is reachable-to-success (issue #1805). Before
/// this, the manifest `panel_ref` stayed Aster's generated no-active-panel
/// placeholder and no durable search generation could ever be built.
///
/// The manifest holds exactly one active panel; the primary constellation panel
/// (timeline) is published here from its authoritative registered contract, and
/// [`CalyxBackend::rebuild_calyx_search_indexes`] republishes the exact
/// requested panel before a rebuild. `publish_active_panel` is idempotent, so
/// this is a no-op once the version is published and never churns `manifest_seq`.
fn ensure_active_panel_published(vault: &SynapseCalyxVault) -> StorageResult<()> {
    let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
    let mut scheduled = BTreeSet::new();
    for &(panel_version, _content_slot) in constellations::SYN_ASSOCIATION_MAINTENANCE_TARGETS {
        if !scheduled.insert(panel_version) {
            return Err(StorageError::WriteFailed {
                cf_name: "calyx_manifest".to_owned(),
                detail: format!(
                    "STORAGE_CALYX_MAINTENANCE_PANEL_DUPLICATE: association-maintenance panel {panel_version} is declared more than once; remediation=keep exactly one scheduled target per panel generation"
                ),
            });
        }
        if syn_queryable_panel_contract(panel_version, created_at_ms)?.is_none() {
            return Err(StorageError::WriteFailed {
                cf_name: "calyx_manifest".to_owned(),
                detail: format!(
                    "STORAGE_CALYX_MAINTENANCE_PANEL_NOT_QUERYABLE: association-maintenance panel {panel_version} has no query-admissible contract; remediation=remove the finite-only panel from SYN_ASSOCIATION_MAINTENANCE_TARGETS or explicitly add a genuinely graded dense lens before scheduling neighbourhood work"
                ),
            });
        }
    }
    let Some(contract) = syn_queryable_panel_contract(SYN_TIMELINE_PANEL_VERSION, created_at_ms)?
    else {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_manifest".to_owned(),
            detail: format!(
                "STORAGE_CALYX_ACTIVE_PANEL_CONTRACT_MISSING: no query-admissible content-slot contract for primary panel generation {SYN_TIMELINE_PANEL_VERSION}; remediation=add the exact reconstructable contract and explicitly admit its graded geometry through syn_queryable_panel_contract before publication"
            ),
        });
    };
    let report = vault
        .publish_active_panel(&contract.panel, &contract.registry)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_manifest",
                &format!("publish active durable panel generation {SYN_TIMELINE_PANEL_VERSION}"),
                &source,
            )
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_ACTIVE_PANEL_PUBLISHED",
        panel_version = report.panel_version,
        published = report.published,
        panel_ref = %report.panel_ref,
        readback_panel_version = report.readback_panel_version,
        "ensured active durable Calyx panel snapshot is published to the manifest"
    );
    Ok(())
}

/// Runs one orphan physical slot-CF retirement pass against a vault runtime
/// under a write guard. The pass itself derives orphan-ness from live Base
/// membership, logs exact row/SST counts, removes fail-closed with readback,
/// and bounds its durable-lock holds to one CF drop at a time (issue #1776,
/// #1806).
fn retire_orphan_slot_cfs_on_vault(
    vault: &CalyxVaultRuntime,
) -> StorageResult<AsterOrphanSlotGcReport> {
    vault.with_vault(
        "<orphan-slot-gc>",
        "retire orphan physical slot column families",
        true,
        |vault| {
            vault.retire_orphan_slot_cfs().map_err(|source| {
                calyx_write_failed(
                    "<orphan-slot-gc>",
                    "retire orphan physical slot column families",
                    &source,
                )
            })
        },
    )
}

/// Retires one superseded panel's published search generation (#1972).
///
/// The physical safety checks (published, not active, removal proven by
/// re-enumeration) belong to the vault. The two checks here are the ones that
/// need the **panel catalog**, and both are refusals rather than warnings:
///
/// 1. A version with a code-declared slot contract is *maintainable*. Its
///    generation can be rebuilt, so retiring it destroys an index that the
///    sweep would have kept current — the remedy for a lagging maintainable
///    generation is `search_rebuild`, never this.
/// 2. A version with no place in any live panel's declared lineage is not
///    known to be superseded; it is simply **unknown**. #1972 ask 2 is exactly
///    this distinction, and collapsing it would let a typo'd or
///    genuinely-unexplained version authorise its own deletion.
///
/// So an operator cannot retire anything except a generation the code already
/// declares to be a closed superseded ancestor of a live panel.
fn retire_search_generation_on_vault(
    vault: &CalyxVaultRuntime,
    panel_version: u32,
) -> StorageResult<(SynapseCalyxRetiredSearchGeneration, SupersededPanelLineage)> {
    let source = format!("panel:{panel_version}");
    vault.with_vault(
        &source,
        "retire a superseded published search generation",
        true,
        |vault| {
            let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
            if syn_queryable_panel_contract(panel_version, created_at_ms)?.is_some() {
                return Err(StorageError::CalyxWriteFailed {
                    cf_name: source.clone(),
                    code: "STORAGE_SEARCH_GENERATION_NOT_RETIRABLE",
                    detail: format!(
                        "panel version {panel_version} has a code-declared slot contract, so its \
                         published search generation is maintainable and can be rebuilt; retiring \
                         it would destroy an index the sweep keeps current"
                    ),
                    remediation:
                        "run storage operation=search_rebuild for this panel instead; retirement is \
                         only for a closed superseded generation",
                    committed_seq: None,
                });
            }
            let Some(lineage) = superseded_panel_lineage(panel_version) else {
                return Err(StorageError::CalyxWriteFailed {
                    cf_name: source.clone(),
                    code: "STORAGE_SEARCH_GENERATION_UNKNOWN_PANEL",
                    detail: format!(
                        "panel version {panel_version} has no code-declared slot contract and no \
                         place in any live panel's declared lineage, so nothing establishes that \
                         it is a superseded generation and it must not be deleted"
                    ),
                    remediation:
                        "investigate what published this generation directory, then declare the \
                         panel's contract or its lineage in builtin_panel_catalog before retiring it",
                    committed_seq: None,
                });
            };
            let report = vault.retire_search_generation(panel_version).map_err(|source| {
                calyx_write_failed(
                    &format!("panel:{panel_version}"),
                    "retire a superseded published search generation",
                    &source,
                )
            })?;
            Ok((report, lineage))
        },
    )
}

/// Panel generations this process has proven are claimed in the Registry-CF
/// allocator (#2062 ask 2).
///
/// A claim is monotone — the allocator never releases an owner row, only adds a
/// retirement record beside it — so a generation proven claimed once stays
/// claimed for the life of the vault, and the durable check is paid once per
/// generation rather than once per Base row. Seeded at vault open by the
/// built-in reservation readback, extended by
/// [`ensure_base_write_generation_claimed`].
static CLAIMED_PANEL_GENERATIONS: std::sync::LazyLock<std::sync::Mutex<BTreeSet<u32>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeSet::new()));

fn remember_claimed_panel_generation(panel_generation: u32) {
    let mut guard = match CLAIMED_PANEL_GENERATIONS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.insert(panel_generation);
}

fn panel_generation_already_proven(panel_generation: u32) -> bool {
    let guard = match CLAIMED_PANEL_GENERATIONS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.contains(&panel_generation)
}

/// Refuses a `Base` write whose panel generation no authority claims (#2062
/// ask 2).
///
/// The allocator's `owners` map is the single authority: a built-in generation
/// is claimed by the boot reservation above, a runtime generation by the
/// allocation that mints it, and nothing else can produce a claim. A row
/// written under a generation absent from it belongs to no panel — no surface
/// reads it, no re-measure rebuilds it, and the only thing that ever notices is
/// a census hours later, which is precisely how 62,566 rows accumulated before
/// anyone asked where they came from.
///
/// Call this wherever a panel version reaches a write **as data**. Constant
/// generations are proven at vault open instead, and runtime generations by
/// their own allocation, so neither pays this.
fn ensure_base_write_generation_claimed(
    vault: &SynapseCalyxVault,
    panel_generation: u32,
) -> StorageResult<()> {
    if panel_generation_already_proven(panel_generation) {
        return Ok(());
    }
    let claim = vault
        .ensure_panel_generation_claimed(panel_generation)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_base",
                "verify the panel generation claim of a Base write",
                &source,
            )
        })?;
    tracing::debug!(
        code = "STORAGE_PANEL_GENERATION_CLAIM_PROVEN",
        panel_generation,
        owner = %claim.owner,
        retired_by = ?claim.retired_by,
        "a Base write's panel generation carries a durable Registry CF ownership claim"
    );
    remember_claimed_panel_generation(panel_generation);
    Ok(())
}

/// Reserves an ownership claim for every generation a compile-time write site
/// can name, taken from [`constellations::builtin_panel_catalog`] itself.
///
/// # Why the catalog and not a list (#2062 ask 2)
///
/// This used to carry its own hand-written array of eleven `(name, version)`
/// pairs, maintained separately from the catalog every other surface reads. Two
/// separately-maintained declarations of the same fact diverge, and this pair
/// had: the three derived-snapshot panels were in the catalog and not in the
/// array, so their generations were claimed only later, by the publish path,
/// and only if a publish happened to run.
///
/// Deriving the list from the catalog is what makes the write-side invariant
/// provable rather than conventional. Every panel version a constant-named
/// write site can reach is claimed here, this runs during vault open, and a
/// failure fails the open — so no writer in the process can name a generation
/// the allocator does not own. Runtime-minted generations are claimed by the
/// allocation that produces them, before the batch that uses them is written,
/// which is the other half of the same invariant.
///
/// Superseded versions are deliberately NOT reserved: nothing writes to them,
/// so an ownership claim would guard nothing. Their claims already exist for
/// every generation that was once active — this function reserved it back when
/// it was — which is what makes the census's lineage cross-check possible
/// (#2093).
///
/// This comment used to end differently. It named
/// `SYN_AGENT_EVENT_PANEL_VERSION_PRE_1983` as an example of a lineage entry
/// that is "a *different* panel's retired generation", carried in
/// `syn-graphpos-app-v1`'s lineage, and called that "a correct declaration". It
/// was not: `1_665_001` was `syn-agent-event-v1`'s own generation, misfiled onto
/// the graph panel by #1983, and the allocator on any vault old enough to hold
/// it says so — it owns that generation as `builtin:syn-agent-event-v1`. The
/// anomaly was visible here first and was explained away rather than checked
/// against the authority sitting one call below. `build_panel_coverage_report`
/// now performs that check every census instead of leaving it to a reader.
fn ensure_builtin_panel_generation_reservations(vault: &SynapseCalyxVault) -> StorageResult<()> {
    let mut reservations: Vec<(String, u32)> = Vec::new();
    for entry in constellations::builtin_panel_catalog() {
        let reservation = (entry.panel_name.to_owned(), entry.panel_version);
        if !reservations.contains(&reservation) {
            reservations.push(reservation);
        }
    }
    let readback = vault
        .reserve_panel_generations(&reservations)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_registry",
                "reserve built-in native Calyx panel generations",
                &source,
            )
        })?;
    // Every reserved generation must read back owned by exactly the panel that
    // reserved it. The old count-only check could be satisfied by unrelated
    // owners — including the runtime-minted ones — so it could not establish
    // the thing the write gate depends on.
    let mut unowned: Vec<String> = Vec::new();
    for (panel_name, generation) in &reservations {
        let expected = format!("builtin:{panel_name}");
        if readback.owners.get(generation) != Some(&expected) {
            unowned.push(format!(
                "{generation} expected={expected:?} actual={:?}",
                readback.owners.get(generation)
            ));
        }
    }
    if !unowned.is_empty() || readback.next_generation <= SYN_MCP_USAGE_PANEL_VERSION {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_registry".to_owned(),
            detail: format!(
                "STORAGE_CALYX_PANEL_GENERATION_READBACK_INVALID: owners={} expected_at_least={} next={} required_above={} unowned={unowned:?}; remediation=inspect native Registry CF allocator ownership before any panel lifecycle mutation",
                readback.owner_count,
                reservations.len(),
                readback.next_generation,
                SYN_MCP_USAGE_PANEL_VERSION
            ),
        });
    }
    for (_, generation) in &reservations {
        remember_claimed_panel_generation(*generation);
    }
    tracing::info!(
        code = "STORAGE_CALYX_PANEL_GENERATIONS_RESERVED",
        owner_count = readback.owner_count,
        operation_count = readback.operation_count,
        next_generation = readback.next_generation,
        latest_seq = readback.latest_seq,
        "built-in Calyx panel generations have exact Registry CF ownership"
    );
    Ok(())
}

/// Open-time durability + writability preparation, WITHOUT a full-vault
/// compaction pass.
///
/// 2026-07-23 cold-start root cause: this previously called
/// `compact_native_fanout_once`, which performs a full catalog scan over every
/// SST plus per-CF compaction and exclusive router refresh — minutes of work
/// serialized on the open path *before the daemon could bind its socket*
/// (observed 14-minute cold start; the `RocksDB` "Speed Up DB Open" guidance is
/// explicit that compaction belongs after open). A checkpoint-only replacement
/// then failed differently on a backlogged vault: the first startup write
/// (activity recorder → `CF_TIMELINE`) hit `CALYX_ASTER_SST_FANOUT_WRITE_STALL`
/// because the inline pass had been the thing clearing write-stall debt. The
/// readiness pass used here is the exact middle ground: it checkpoints staged
/// batches (preserving the #1132 invariant), compacts ONLY CFs at/above the
/// write-stall trigger, and verifies the hard range-page source limit — a
/// steady-state boot pays one catalog scan; routine/tiny-file drain lanes stay
/// owned by the periodic GC task on the blocking maintenance pool.
fn prepare_calyx_open_fanout(
    vault: &SynapseCalyxVault,
    path: &Path,
    phase: &'static str,
) -> StorageResult<()> {
    let readiness = vault
        .compact_write_stall_readiness_once()
        .map_err(|source| {
            calyx_open_failed_detail(
                path,
                format!("checkpoint + write-stall readiness compaction during {phase}: {source}"),
            )
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_OPEN_CHECKPOINT_READY",
        phase,
        attempted_cfs = readiness.attempted_cfs,
        compacted_cfs = readiness.compacted_cfs,
        reclaimed_input_files = readiness.reclaimed_input_files,
        compacted_cf_names = ?readiness.compacted_cf_names,
        "checkpointed staged durable batches and cleared write-stalled CFs during \
         storage open; routine native-CF compaction deferred to the periodic GC task"
    );
    Ok(())
}

struct CalyxPressureCompaction {
    vault: Arc<CalyxVaultRuntime>,
}

impl CalyxPressureCompaction {
    const fn new(vault: Arc<CalyxVaultRuntime>) -> Self {
        Self { vault }
    }
}

impl pressure::PressureCompaction for CalyxPressureCompaction {
    fn compact_native_for_pressure(&self) -> StorageResult<Vec<&'static str>> {
        self.vault.with_vault(
            pressure::PRESSURE_CF,
            "compact Calyx KV for pressure",
            true,
            |vault| {
                let compacted = vault.compact_kv_once().map_err(|source| {
                    calyx_write_failed(
                        pressure::PRESSURE_CF,
                        "compact Calyx KV for pressure",
                        &source,
                    )
                })?;
                if compacted {
                    Ok(cf::ALL_COLUMN_FAMILIES.to_vec())
                } else {
                    Ok(Vec::new())
                }
            },
        )
    }
}

/// Periodic checkpoint-only runner (2026-07-23 cold-start fix): advances the
/// manifest `durable_seq` floor every `CALYX_CHECKPOINT_INTERVAL` so a daemon
/// kill strands at most one interval of WAL tail instead of one 5-minute GC
/// interval (~20k sequences observed, replaying for minutes on every boot).
/// Checkpointing writes only the batches staged since the previous flush plus
/// one manifest publish — it never scans catalogs or compacts.
/// Drives the unattended derived-state maintainer on its own cadence.
///
/// It carries no vault handle of its own: the pass reads the registered process
/// storage handle through a `Weak`, exactly like the guard-threshold lowering
/// publisher, so a closed vault is observed as a recorded skip rather than kept
/// alive by the scheduler.
#[derive(Default)]
struct CalyxDerivedStateRunner {
    association_catch_up_next: AtomicBool,
}

impl gc::GcRunner for CalyxDerivedStateRunner {
    /// Returns the tick's real outcome, so `STORAGE_MAINTENANCE_COMPLETED
    /// operation="storage_derived_state" is_ok=…` is a measurement (#2088).
    ///
    /// This used to discard a `()` and return `Ok(gc::GcReport::default())`
    /// unconditionally, which made `is_ok` a tautology and left the maintenance
    /// log and the retry classification dead for this one task while `storage_gc`
    /// and `storage_checkpoint` used both.
    ///
    /// The report stays empty, and is now *honestly* empty: a derived-state tick
    /// evicts no rows and takes no deletion decision, so it has no CF reports and
    /// no source census. `gc::mark_gc_tick_completed` no longer manufactures
    /// zeroed `last_successful_*` CF aggregates from a report that carries none
    /// (#2088 ask 3) — the same discipline `source_census` already had.
    fn run_once(&self) -> StorageResult<gc::GcReport> {
        let catch_up_only = self.association_catch_up_next.swap(false, Ordering::AcqRel);
        if catch_up_only {
            crate::derived_state::run_association_weave_catch_up()?;
        } else {
            crate::derived_state::run_derived_state_maintenance()?;
        }
        self.association_catch_up_next.store(
            crate::derived_state::association_weave_backlog_pending(),
            Ordering::Release,
        );
        Ok(gc::GcReport::default())
    }

    fn successful_continuation_delay(&self) -> Option<Duration> {
        self.association_catch_up_next
            .load(Ordering::Acquire)
            .then_some(crate::derived_state::ASSOCIATION_BACKLOG_CONTINUATION_DELAY)
    }
}

struct CalyxCheckpointRunner {
    vault: Arc<CalyxVaultRuntime>,
}

impl CalyxCheckpointRunner {
    const fn new(vault: Arc<CalyxVaultRuntime>) -> Self {
        Self { vault }
    }
}

impl gc::GcRunner for CalyxCheckpointRunner {
    fn run_once(&self) -> StorageResult<gc::GcReport> {
        let rebase = self.vault.with_vault(
            CALYX_CHECKPOINT_CF,
            "periodic durable checkpoint",
            true,
            |vault| {
                vault
                    .checkpoint_with_snapshot_delta_rebase()
                    .map_err(|source| {
                    calyx_write_failed(
                        CALYX_CHECKPOINT_CF,
                        "materialize staged durable checkpoints, advance the manifest floor, and rebase the process-local snapshot delta",
                        &source,
                    )
                })
            },
        )?;
        if rebase.rebased {
            let release =
                synapse_calyx::release_process_memory("periodic checkpoint snapshot-delta rebase")
                    .map_err(|source| {
                        calyx_write_failed(
                            CALYX_CHECKPOINT_CF,
                            "return retired snapshot-delta pages to the operating system",
                            &source,
                        )
                    })?;
            tracing::info!(
                code = "STORAGE_CALYX_SNAPSHOT_DELTA_REBASE_COMPLETED",
                previous_floor_seq = rebase.previous_floor_seq,
                new_floor_seq = rebase.new_floor_seq,
                flushed_ssts = rebase.flushed_ssts,
                before_keys = rebase.before_keys,
                before_versions = rebase.before_versions,
                before_payload_bytes = rebase.before_payload_bytes,
                after_keys = rebase.after_keys,
                after_versions = rebase.after_versions,
                after_payload_bytes = rebase.after_payload_bytes,
                allocator_before_private_bytes = release.private_bytes_before,
                allocator_after_private_bytes = release.private_bytes_after,
                allocator_released_private_bytes = release.private_bytes_reclaimed,
                allocator_release_elapsed_us = release.elapsed_us,
                "checkpoint installed the immutable serving baseline, retired the redundant MVCC journal, and read back process memory"
            );
        }
        Ok(gc::GcReport::default())
    }
}

/// Highest budget multiplier a memory-pressure escalation may reach.
///
/// 4x the base pass budget is 8 s of reclamation inside a 300 s maintenance
/// tick — a 2.7% duty cycle, and the per-shard guard-hold budget is *not*
/// scaled, so the worst case a commit can see is unchanged no matter how
/// escalated the pass is. The ceiling exists because "run until the debt is
/// gone" is not a bound: a pass that cannot converge inside 8 s is reporting a
/// problem (a pinned floor, a write rate the vault cannot sustain) that running
/// longer would hide rather than fix.
const MAX_SNAPSHOT_VERSION_GC_ESCALATION: u32 = 4;

/// Memory-pressure state carried across snapshot-version GC passes (#2122).
///
/// The trigger is deliberately *relative*, not an absolute byte threshold: an
/// absolute one would have to be guessed, would be wrong on every machine with
/// a different corpus, and would sit at whatever value made the graph look right
/// on the day it was written. This escalates only on evidence the daemon
/// produced itself — the previous pass did not finish its sweep **and** private
/// commit did not fall — and de-escalates the moment a sweep completes, which is
/// the point at which there is nothing left to chase.
#[derive(Debug)]
struct SnapshotVersionGcPressure {
    factor: u32,
    last_private_bytes: Option<u64>,
    last_sweep_completed: bool,
}

impl Default for SnapshotVersionGcPressure {
    fn default() -> Self {
        Self {
            factor: 1,
            last_private_bytes: None,
            last_sweep_completed: true,
        }
    }
}

impl SnapshotVersionGcPressure {
    /// Chooses this pass's budget multiplier from the previous pass's outcome
    /// and the private-commit trend.
    fn plan(&mut self, private_bytes_now: u64) -> u32 {
        let grew = self
            .last_private_bytes
            .is_none_or(|previous| private_bytes_now >= previous);
        self.factor = if self.last_sweep_completed {
            // Nothing was left over, so nothing needs chasing. Reset rather than
            // decay: a completed sweep is proof, not a trend.
            1
        } else if grew {
            self.factor
                .saturating_mul(2)
                .min(MAX_SNAPSHOT_VERSION_GC_ESCALATION)
        } else {
            // Sweep incomplete but memory is falling: the current budget is
            // already winning. Holding it steady avoids escalating into work the
            // daemon is not asking for.
            self.factor
        };
        self.factor
    }

    const fn record(&mut self, private_bytes_after: u64, sweep_completed: bool) {
        self.last_private_bytes = Some(private_bytes_after);
        self.last_sweep_completed = sweep_completed;
    }
}

struct CalyxGcRunner {
    vault: Arc<CalyxVaultRuntime>,
    snapshot_version_gc: Mutex<SnapshotVersionGcPressure>,
    /// Exact derived-source reachability owned by the vault's single GC
    /// authority. Scheduled and operator-triggered passes serialize through
    /// this same cache, so the process never retains or rebuilds two copies of
    /// the corpus-sized protection set.
    source_census: Mutex<Option<CalyxGcSourceCensusCache>>,
}

impl CalyxGcRunner {
    fn new(vault: Arc<CalyxVaultRuntime>) -> Self {
        Self {
            vault,
            snapshot_version_gc: Mutex::new(SnapshotVersionGcPressure::default()),
            source_census: Mutex::new(None),
        }
    }

    /// Runs one bounded in-RAM MVCC version-chain reclamation pass (#2122).
    ///
    /// # Errors
    ///
    /// Fails closed. There is no "reclamation is best-effort" branch here on
    /// purpose: the defect this fixes is a reclaimer that never ran while every
    /// surrounding indicator reported health, and swallowing its failures would
    /// rebuild exactly that. A vault that is closing refuses with
    /// `CALYX_ASTER_VAULT_CLOSING`, which the maintenance loop already
    /// classifies as fencing rather than a fault.
    fn reclaim_snapshot_versions_once(&self) -> StorageResult<gc::SnapshotVersionGcPassReport> {
        let base =
            synapse_calyx::SynapseCalyxSnapshotVersionGcBudget::from_env().map_err(|source| {
                StorageError::BackendInvalidConfig {
                    value: "CALYX_SNAPSHOT_VERSION_GC_*".to_owned(),
                    detail: format!("read snapshot-version GC budget: {source}"),
                }
            })?;
        let private_bytes_before = calyx_process_private_bytes()?;
        // A poisoned pressure lock means an earlier pass panicked while holding
        // it. Reclamation still runs — refusing it would trade a memory leak for
        // a panic's aftermath — but at the unescalated base budget, and it says
        // so rather than quietly reporting a factor it did not compute.
        let escalation_factor = self.snapshot_version_gc.lock().map_or_else(
            |_poisoned| {
                tracing::warn!(
                    code = "STORAGE_SNAPSHOT_VERSION_GC_PRESSURE_POISONED",
                    private_bytes_before,
                    "snapshot-version GC pressure state is poisoned by an earlier panic; running                      this pass at the base budget with no memory-pressure escalation"
                );
                1
            },
            |mut state| state.plan(private_bytes_before),
        );
        let budget = base.scaled(escalation_factor);
        let pass = self.vault.with_vault(
            CALYX_GC_CF,
            "reclaim MVCC snapshot versions",
            true,
            |vault| {
                vault
                    .reclaim_snapshot_versions_once(budget)
                    .map_err(|source| {
                        calyx_write_failed(
                            CALYX_GC_CF,
                            "reclaim in-RAM MVCC snapshot version chains",
                            &source,
                        )
                    })
            },
        )?;
        let private_bytes_after = calyx_process_private_bytes()?;
        match self.snapshot_version_gc.lock() {
            Ok(mut state) => state.record(private_bytes_after, pass.sweep_completed),
            Err(_poisoned) => tracing::warn!(
                code = "STORAGE_SNAPSHOT_VERSION_GC_PRESSURE_POISONED",
                private_bytes_after,
                sweep_completed = pass.sweep_completed,
                "could not record this pass's outcome into the pressure state; the next pass will                  plan from a stale sample"
            ),
        }
        let report = gc::SnapshotVersionGcPassReport {
            pass,
            private_bytes_before,
            private_bytes_after,
            escalation_factor,
            budget_max_versions: budget.max_versions,
            budget_max_pass_us: budget.max_pass_us,
        };
        tracing::info!(
            code = "STORAGE_SNAPSHOT_VERSION_GC_PASS",
            floor_seq = report.pass.floor_seq,
            current_seq = report.pass.current_seq,
            active_leases = report.pass.active_leases,
            versions_reclaimed = report.pass.versions_reclaimed,
            bytes_reclaimed = report.pass.bytes_reclaimed,
            chains_compacted = report.pass.chains_compacted,
            chains_scanned = report.pass.chains_scanned,
            shards_visited = report.pass.shards_visited,
            shards_total = report.pass.shards_total,
            shard_guard_holds = report.pass.shard_guard_holds,
            sweep_completed = report.pass.sweep_completed,
            stopped_on = report.pass.stopped_on.as_str(),
            elapsed_us = report.pass.elapsed_us,
            max_shard_hold_us = report.pass.max_shard_hold_us,
            private_bytes_before,
            private_bytes_after,
            escalation_factor,
            budget_max_versions = budget.max_versions,
            budget_max_pass_us = budget.max_pass_us,
            "reclaimed snapshot-obsolete in-RAM MVCC version chains"
        );
        Ok(report)
    }

    /// Logical row-cap eviction followed by in-RAM MVCC version reclamation.
    ///
    /// The local order is load-bearing for same-tick debt convergence: eviction
    /// deletes by **writing tombstones**, and every tombstone is a commit that
    /// appends another full value clone to another version chain (#2122 — the
    /// GC we were already running made the leak grow faster). Reclaiming
    /// afterwards can sweep this pass's own tombstone versions instead of
    /// leaving them resident until a later cadence.
    ///
    /// This sequence is deliberately one operation; the global maintenance
    /// permit count does not order operation classes (#2150). Concurrent
    /// foreground commits remain correct because commit and reclamation take
    /// the same MVCC row-shard write guard. A future pressure path that needs
    /// logical eviction must route through this sequence rather than adding
    /// logical mutation to `PressureCompaction`.
    fn run_full_once(&self) -> StorageResult<gc::GcReport> {
        let mut report = self.run_default_once()?;
        report.snapshot_version_gc = Some(self.reclaim_snapshot_versions_once()?);
        Ok(report)
    }

    /// Caller-selected logical row-cap eviction followed by the same physical
    /// snapshot-version reclamation that the scheduled full pass runs.
    fn run_full_once_with_row_cap(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<gc::GcReport> {
        let mut report = self.run_row_cap_once(cf_name, soft_cap_rows, hard_cap_rows)?;
        report.snapshot_version_gc = Some(self.reclaim_snapshot_versions_once()?);
        Ok(report)
    }

    fn run_default_once(&self) -> StorageResult<gc::GcReport> {
        let budgets = calyx_gc_default_budgets()?;
        self.run_with_budgets(&budgets)
    }

    fn run_row_cap_once(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<gc::GcReport> {
        let budget = calyx_gc_row_budget(cf_name, soft_cap_rows, hard_cap_rows)?;
        if budget.protected {
            return Err(calyx_write_failed_detail(
                cf_name,
                format!(
                    "STORAGE_CALYX_GC_PROTECTED_CF_POLICY: cf={cf_name} unit={} soft_cap={} hard_cap={}; generic GC has no deletion authority for this family, so the request was rejected before any source census or CF scan; remediation=use the typed retention owner declared for this family",
                    budget.unit.as_str(),
                    budget.soft_cap,
                    budget.hard_cap
                ),
            ));
        }
        self.run_with_budgets(std::slice::from_ref(&budget))
    }

    fn run_with_budgets(&self, budgets: &[CalyxGcBudget]) -> StorageResult<gc::GcReport> {
        self.vault
            .with_vault(CALYX_GC_CF, "run Calyx GC", true, |vault| {
                run_calyx_gc_budgets(vault, budgets, &self.source_census)
            })
    }
}

impl gc::GcRunner for CalyxGcRunner {
    /// The `storage_gc` maintenance tick.
    ///
    /// # Why reclamation lives on this tick and not on one of its own
    ///
    /// Ordering (see [`CalyxGcRunner::run_full_once`]) is one reason; admission
    /// is the other. Riding this tick inherits its whole discipline for free:
    /// one blocking-pool permit, one retry classification, one
    /// `STORAGE_MAINTENANCE_COMPLETED` record, one deferral budget against the
    /// shared Calyx maintenance lock. A fourth periodic task would have had to
    /// duplicate all of it to say the same thing, and would have had to
    /// negotiate with this one for the lock while doing so.
    fn run_once(&self) -> StorageResult<gc::GcReport> {
        self.run_full_once()
    }
}

fn put_observation_with_lifecycle(
    vault: &SynapseCalyxVault,
    panel_name: &str,
    identity_input: &[u8],
    source_bytes: &[u8],
    constellation: Constellation,
) -> StorageResult<SynapseCalyxObservationPutReadback> {
    let lifecycle = vault
        .materialize_panel_lifecycle_generation(
            panel_name,
            identity_input,
            source_bytes,
            &constellation,
        )
        .map_err(|source| {
            calyx_write_failed(
                "calyx_constellation",
                "materialize latest panel lifecycle generation",
                &source,
            )
        })?;
    let Some(lifecycle) = lifecycle else {
        return vault
            .put_observation_constellation(constellation)
            .map_err(|source| {
                calyx_write_failed(
                    "calyx_constellation",
                    "put native Calyx observation constellation",
                    &source,
                )
            });
    };
    if lifecycle.cx_id == constellation.cx_id {
        return vault
            .put_observation_constellation(constellation)
            .map_err(|source| {
                calyx_write_failed(
                    "calyx_constellation",
                    "put current panel lifecycle constellation",
                    &source,
                )
            });
    }
    let mut readbacks = vault
        .put_observation_constellation_batch([constellation, lifecycle])
        .map_err(|source| {
            calyx_write_failed(
                "calyx_constellation",
                "atomically put base and latest panel lifecycle constellations",
                &source,
            )
        })?;
    if readbacks.len() != 2 {
        return Err(calyx_write_failed_detail(
            "calyx_constellation",
            format!(
                "atomic lifecycle dual measurement returned {} readbacks instead of 2",
                readbacks.len()
            ),
        ));
    }
    Ok(readbacks.remove(0))
}

fn lifecycle_base_constellation_from_source(
    vault: &SynapseCalyxVault,
    panel_name: &str,
    source_panel_version: u32,
    source_cf: &str,
    source_key: &[u8],
    raw: &[u8],
) -> StorageResult<(Vec<u8>, Constellation)> {
    let expected_source_cf = match source_panel_version {
        SYN_TIMELINE_PANEL_VERSION => cf::CF_TIMELINE,
        SYN_EPISODE_PANEL_VERSION => cf::CF_EPISODES,
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION => cf::CF_AGENT_TRANSCRIPTS,
        SYN_MCP_USAGE_PANEL_VERSION => cf::CF_KV,
        _ => {
            return Err(StorageError::BackendInvalidConfig {
                value: source_panel_version.to_string(),
                detail: "the lifecycle worker has no authoritative typed source builder for this active panel generation".to_owned(),
            });
        }
    };
    if source_cf != expected_source_cf {
        return Err(StorageError::ReadFailed {
            cf_name: source_cf.to_owned(),
            detail: format!(
                "lifecycle Base pointer names {source_cf}, but panel {source_panel_version} requires {expected_source_cf}"
            ),
        });
    }
    let panel = constellations::anchor_panel_for_source_row(expected_source_cf, source_key)?;
    if panel.panel_name != panel_name || panel.panel_version != source_panel_version {
        return Err(calyx_write_failed_detail(
            "calyx_constellation",
            format!(
                "lifecycle source {source_cf}/{} maps to {} generation {}, expected {panel_name} generation {source_panel_version}",
                constellations::hex_encode(source_key),
                panel.panel_name,
                panel.panel_version
            ),
        ));
    }
    let identity = constellations::source_constellation_input_bytes(
        panel.input_mode,
        source_cf,
        source_key,
        raw,
    );
    let context = NativeConstellationContext {
        vault_id: vault.vault_id_value(),
        cx_id: vault.cx_id_for_input(&identity, source_panel_version),
        created_at_ms: calyx_clock_now_for_write(vault, source_cf)?,
        next_ledger_seq: vault.latest_seq().saturating_add(1),
    };
    let base = match source_panel_version {
        SYN_TIMELINE_PANEL_VERSION => {
            let record: TimelineRecord =
                serde_json::from_slice(raw).map_err(|source| StorageError::DecodeJson {
                    type_name: "panel_lifecycle_timeline",
                    source,
                })?;
            constellations::build_timeline_constellation(context, source_key, raw, &record)?
        }
        SYN_EPISODE_PANEL_VERSION => {
            let record: EpisodeRecord =
                serde_json::from_slice(raw).map_err(|source| StorageError::DecodeJson {
                    type_name: "panel_lifecycle_episode",
                    source,
                })?;
            constellations::build_episode_constellation(context, source_key, raw, &record)?
        }
        SYN_AGENT_TRANSCRIPT_PANEL_VERSION => {
            let record: AgentTranscriptRecord =
                serde_json::from_slice(raw).map_err(|source| StorageError::DecodeJson {
                    type_name: "panel_lifecycle_agent_transcript",
                    source,
                })?;
            constellations::build_agent_transcript_constellation(context, source_key, raw, &record)?
        }
        SYN_MCP_USAGE_PANEL_VERSION => {
            let record: Value =
                serde_json::from_slice(raw).map_err(|source| StorageError::DecodeJson {
                    type_name: "panel_lifecycle_mcp_usage",
                    source,
                })?;
            constellations::build_mcp_usage_constellation(
                context, source_key, raw, &identity, &record,
            )?
        }
        _ => {
            return Err(StorageError::BackendInvalidConfig {
                value: source_panel_version.to_string(),
                detail: "the lifecycle worker has no authoritative typed source builder for this active panel generation".to_owned(),
            });
        }
    };
    Ok((identity, base))
}

fn resolve_queryable_panel_contract(
    vault: &SynapseCalyxVault,
    panel_version: u32,
    created_at_ms: u64,
) -> StorageResult<Option<SynapseCalyxPanelState>> {
    if let Some(contract) = syn_queryable_panel_contract(panel_version, created_at_ms)? {
        return Ok(Some(SynapseCalyxPanelState {
            panel: contract.panel,
            registry: contract.registry,
            registry_snapshot: None,
        }));
    }
    let mut matched = None;
    for entry in constellations::builtin_panel_catalog() {
        let base_registry =
            match syn_reconstructable_panel_contract(entry.panel_version, created_at_ms)? {
                Some(base) => base.registry,
                None if matches!(
                    entry.panel_name,
                    constellations::SYN_GRAPHPOS_APP_PANEL_NAME
                        | constellations::SYN_GRAPHPOS_PROCESS_PANEL_NAME
                        | constellations::SYN_PATH_HIERARCHY_PANEL_NAME
                ) =>
                {
                    calyx_registry::Registry::new()
                }
                None => continue,
            };
        let Some(candidate) = vault
            .reconstruct_panel_lifecycle_contract(entry.panel_name, base_registry)
            .map_err(|source| {
                calyx_read_failed(
                    "calyx_registry",
                    "reconstruct durable panel lifecycle contract",
                    &source,
                )
            })?
        else {
            continue;
        };
        if candidate.panel.version != panel_version {
            continue;
        }
        assert_panel_carries_graded_dense_lens(
            candidate.panel.version,
            &candidate.panel.slots,
            &candidate.registry,
        )?;
        if matched.is_some() {
            return Err(StorageError::ReadFailed {
                cf_name: "calyx_registry".to_owned(),
                detail: format!(
                    "multiple durable lifecycle panels claim generation {panel_version}; generation identity is ambiguous"
                ),
            });
        }
        matched = Some(candidate);
    }
    Ok(matched)
}

impl StorageBackend for CalyxBackend {
    fn kind(&self) -> StorageBackendKind {
        StorageBackendKind::Calyx
    }

    fn put_batch(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()> {
        calyx_collection_id_for_cf_write(cf_name)?;
        if rows.is_empty() {
            return Ok(());
        }
        if !self.pressure.permits_write(cf_name) {
            let pressure_level = format!("{:?}", self.pressure.level());
            synapse_telemetry::metrics::counter!(
                STORAGE_WRITES_SHED_TOTAL,
                "cf" => cf_name.to_owned()
            )
            .increment(rows.len() as u64);
            tracing::warn!(
                code = error_codes::STORAGE_WRITE_FAILED,
                cf = cf_name,
                pressure_level = ?self.pressure.level(),
                dropped_rows = rows.len(),
                metric_name = STORAGE_WRITES_SHED_TOTAL,
                backend = self.kind().as_str(),
                "storage write dropped under disk pressure"
            );
            return Err(StorageError::WriteShed {
                cf_name: cf_name.to_owned(),
                pressure_level,
                rows: rows.len(),
            });
        }
        self.put_batch_pressure_bypass(cf_name, rows)
    }

    fn put_batch_pressure_bypass(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()> {
        let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
        self.with_vault(cf_name, "write Calyx KV batch", true, |vault| {
            let now_ms = calyx_clock_now_for_write(vault, cf_name)?;
            let rows = rows
                .into_iter()
                .map(|(key, value)| calyx_put_row(cf_name, collection_id, &key, &value, now_ms))
                .collect::<StorageResult<Vec<_>>>()?;
            // Retention scans and cap eviction are GC work. Keeping foreground
            // writes bounded prevents MCP handshakes from inheriting large-CF
            // maintenance latency.
            commit_calyx_rows_to_vault(vault, cf_name, rows)
        })
    }

    fn put_cf_batches_pressure_bypass(&self, batches: Vec<OwnedCfWriteBatch>) -> StorageResult<()> {
        if batches.iter().all(|(_cf_name, rows)| rows.is_empty()) {
            return Ok(());
        }
        self.with_vault(
            "<multi-cf>",
            "write Calyx multi-CF KV batch",
            true,
            |vault| {
                let now_ms = calyx_clock_now_for_write(vault, "<multi-cf>")?;
                let mut writes = Vec::new();
                for (cf_name, rows) in batches {
                    let collection_id = calyx_collection_id_for_cf_write(&cf_name)?;
                    for (key, value) in rows {
                        writes.push(calyx_put_row(
                            &cf_name,
                            collection_id,
                            &key,
                            &value,
                            now_ms,
                        )?);
                    }
                }
                commit_calyx_rows_atomically_to_vault(vault, "<multi-cf>", writes)
            },
        )
    }

    fn put_cf_batches_if_revisions_pressure_bypass(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
    ) -> StorageResult<RevisionGuardedMutationOutcome> {
        validate_cross_cf_revision_guarded_put(&guards, &batches)?;
        self.with_vault(
            "<multi-cf>",
            "write revision-guarded Calyx multi-CF KV batch",
            true,
            |vault| {
                let now_ms = calyx_clock_now_for_write(vault, "<multi-cf>")?;
                let mut physical_guards = Vec::with_capacity(guards.len());
                for guard in &guards {
                    let collection_id = calyx_collection_id_for_cf_write(&guard.cf_name)?;
                    let physical_key =
                        encode_calyx_key_for_write(&guard.cf_name, collection_id, &guard.key)?;
                    physical_guards.push(SynapseCalyxRevisionGuard::new(
                        ColumnFamily::Kv,
                        physical_key,
                        guard.expected_revision_sha256,
                    ));
                }
                let mut writes = Vec::new();
                for (cf_name, rows) in &batches {
                    let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
                    for (key, value) in rows {
                        writes.push(calyx_put_row(cf_name, collection_id, key, value, now_ms)?);
                    }
                }
                commit_calyx_cross_cf_rows_if_revisions(vault, &guards, &physical_guards, writes)
            },
        )
    }

    fn put_cf_batches_with_expiry_if_revisions_pressure_bypass(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatchWithExpiry>,
    ) -> StorageResult<RevisionGuardedMutationOutcome> {
        let validation_batches = batches
            .iter()
            .map(|(cf_name, rows)| {
                (
                    cf_name.clone(),
                    rows.iter()
                        .map(|row| (row.key.clone(), row.value.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        validate_cross_cf_revision_guarded_put(&guards, &validation_batches)?;
        self.with_vault(
            "<multi-cf>",
            "write revision-guarded Calyx multi-CF KV batch with preserved expiry",
            true,
            |vault| {
                let now_ms = calyx_clock_now_for_write(vault, "<multi-cf>")?;
                let mut physical_guards = Vec::with_capacity(guards.len());
                for guard in &guards {
                    let collection_id = calyx_collection_id_for_cf_write(&guard.cf_name)?;
                    let physical_key =
                        encode_calyx_key_for_write(&guard.cf_name, collection_id, &guard.key)?;
                    physical_guards.push(SynapseCalyxRevisionGuard::new(
                        ColumnFamily::Kv,
                        physical_key,
                        guard.expected_revision_sha256,
                    ));
                }
                let mut writes = Vec::new();
                for (cf_name, rows) in &batches {
                    let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
                    for row in rows {
                        writes.push(calyx_put_row_with_expiry(
                            cf_name,
                            collection_id,
                            row,
                            now_ms,
                        )?);
                    }
                }
                commit_calyx_cross_cf_rows_if_revisions(vault, &guards, &physical_guards, writes)
            },
        )
    }

    fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        self.with_vault(cf_name, "read Calyx KV row", false, |vault| {
            let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
            let key = encode_calyx_key_for_read(cf_name, collection_id, key)?;
            let value = vault
                .read_cf_latest(ColumnFamily::Kv, &key)
                .map_err(|source| {
                    calyx_read_failed(cf_name, "read latest Calyx CF row", &source)
                })?;
            let now_ms = calyx_clock_now_for_read(vault, cf_name)?;
            value.map_or(Ok(None), |bytes| {
                decode_calyx_value_for_read(cf_name, &bytes, now_ms)
            })
        })
    }

    fn get_cf_revisioned(
        &self,
        cf_name: &str,
        key: &[u8],
    ) -> StorageResult<Option<RevisionedRawValue>> {
        self.with_vault(cf_name, "read revisioned Calyx KV row", false, |vault| {
            let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
            let physical_key = encode_calyx_key_for_read(cf_name, collection_id, key)?;
            let physical = vault
                .read_cf_latest_revisioned(ColumnFamily::Kv, &physical_key)
                .map_err(|source| {
                    calyx_read_failed(cf_name, "read latest revisioned Calyx CF row", &source)
                })?;
            let now_ms = calyx_clock_now_for_read(vault, cf_name)?;
            physical.map_or(Ok(None), |physical| {
                let envelope = decode_calyx_value_raw(&physical.value).map_err(|detail| {
                    tracing::error!(
                        code = error_codes::STORAGE_READ_FAILED,
                        cf = cf_name,
                        detail,
                        "Calyx storage backend rejected malformed revisioned KV retention envelope"
                    );
                    StorageError::ReadFailed {
                        cf_name: cf_name.to_owned(),
                        detail,
                    }
                })?;
                Ok(Some(RevisionedRawValue {
                    value: (!calyx_value_is_expired(envelope.expires_at_ms, now_ms))
                        .then(|| envelope.payload.to_vec()),
                    revision_sha256: physical.revision_sha256,
                    expires_at_ms: envelope.expires_at_ms,
                    written_at_ms: envelope.written_at_ms,
                }))
            })
        })
    }

    fn open_calyx_storage_snapshot(
        &self,
        max_age_ms: u64,
    ) -> StorageResult<CalyxStorageSnapshotLease> {
        if !(CALYX_STORAGE_SNAPSHOT_MIN_AGE_MS..=CALYX_STORAGE_SNAPSHOT_MAX_AGE_MS)
            .contains(&max_age_ms)
        {
            return Err(StorageError::ReadFailed {
                cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                detail: format!(
                    "SYNAPSE_CALYX_SNAPSHOT_LEASE_AGE_INVALID: max_age_ms={max_age_ms} is outside {CALYX_STORAGE_SNAPSHOT_MIN_AGE_MS}..={CALYX_STORAGE_SNAPSHOT_MAX_AGE_MS}; remediation=request a short bounded reader lease inside the reported range"
                ),
            });
        }
        self.with_vault(
            "calyx_mvcc_snapshot_leases",
            "open bounded Calyx MVCC storage snapshot",
            false,
            |vault| {
                let opened_at_unix_ms = calyx_clock_now_for_read(
                    vault,
                    "calyx_mvcc_snapshot_leases",
                )?;
                self.prune_expired_storage_snapshots(vault, opened_at_unix_ms)?;
                let mut snapshots = self.lock_storage_snapshots()?;
                if snapshots.len() >= CALYX_STORAGE_SNAPSHOT_MAX_ACTIVE {
                    return Err(StorageError::ReadFailed {
                        cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                        detail: format!(
                            "SYNAPSE_CALYX_SNAPSHOT_LEASE_CAPACITY_EXHAUSTED: {} live historical readers already pin MVCC versions; remediation=release an existing lease or wait for its bounded expiry before opening another",
                            snapshots.len()
                        ),
                    });
                }
                let snapshot = vault
                    .pin_reader(Freshness::FreshDerived, max_age_ms)
                    .map_err(|source| {
                        calyx_read_failed(
                            "calyx_mvcc_snapshot_leases",
                            "pin bounded Calyx MVCC storage snapshot",
                            &source,
                        )
                    })?;
                let lease_id = snapshot.lease().id();
                let expires_at_unix_ms = snapshot.lease().expires_at();
                let active_lease_count = match snapshots.entry(lease_id) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(PinnedStorageSnapshot {
                            snapshot,
                            opened_at_unix_ms,
                        });
                        // The admission check above bounds this value to 64.
                        snapshots.len() as u64
                    }
                    std::collections::btree_map::Entry::Occupied(_entry) => {
                        drop(snapshots);
                        let released = vault.release_reader(lease_id);
                        tracing::error!(
                            code = "SYNAPSE_CALYX_SNAPSHOT_LEASE_ID_COLLISION",
                            lease_id,
                            released,
                            "Aster monotonic reader lease allocator reused a live public lease id"
                        );
                        return Err(StorageError::ReadFailed {
                            cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                            detail: format!(
                                "SYNAPSE_CALYX_SNAPSHOT_LEASE_ID_COLLISION: lease_id={lease_id} already exists in the process-local lease table; remediation=preserve daemon logs and restart the repo-built daemon because the monotonic Aster lease allocator regressed"
                            ),
                        });
                    }
                };
                drop(snapshots);
                Ok(CalyxStorageSnapshotLease {
                    lease_id,
                    snapshot_seq: snapshot.seq(),
                    opened_at_unix_ms,
                    expires_at_unix_ms,
                    max_age_ms,
                    active_lease_count,
                })
            },
        )
    }

    fn read_calyx_storage_snapshot(
        &self,
        lease_id: u64,
        cf_name: &str,
        key: &[u8],
    ) -> StorageResult<CalyxStorageSnapshotReadback> {
        if lease_id == 0 {
            return Err(StorageError::ReadFailed {
                cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                detail: "SYNAPSE_CALYX_SNAPSHOT_LEASE_ID_INVALID: lease_id must be nonzero; remediation=pass the exact lease_id returned by snapshot_open".to_owned(),
            });
        }
        self.with_vault(cf_name, "read addressed row through Calyx MVCC snapshot", false, |vault| {
            let now_ms = calyx_clock_now_for_read(vault, cf_name)?;
            let pinned = {
                let mut snapshots = self.lock_storage_snapshots()?;
                let Some(pinned) = snapshots.get(&lease_id).copied() else {
                    return Err(StorageError::ReadFailed {
                        cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                        detail: format!(
                            "SYNAPSE_CALYX_SNAPSHOT_LEASE_UNKNOWN: lease_id={lease_id} is not active in this daemon; remediation=open a new snapshot and use its process-local lease_id before the daemon restarts or the lease expires"
                        ),
                    });
                };
                if pinned.snapshot.lease().expires_at() <= now_ms {
                    snapshots.remove(&lease_id);
                    drop(snapshots);
                    let _ = vault.release_reader(lease_id);
                    return Err(StorageError::ReadFailed {
                        cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                        detail: format!(
                            "SYNAPSE_CALYX_SNAPSHOT_LEASE_EXPIRED: lease_id={lease_id} expired_at_unix_ms={} read_at_unix_ms={now_ms}; remediation=open a new bounded snapshot and complete historical reads before its reported expiry",
                            pinned.snapshot.lease().expires_at()
                        ),
                    });
                }
                pinned
            };
            let (physical, native_calyx_row) =
                read_calyx_storage_snapshot_target(vault, pinned, cf_name, key)?;
            let expires_at_unix_ms = pinned.snapshot.lease().expires_at();
            let current_seq = vault.latest_seq();
            let Some(physical) = physical else {
                return Ok(CalyxStorageSnapshotReadback {
                    lease_id,
                    snapshot_seq: pinned.snapshot.seq(),
                    current_seq,
                    opened_at_unix_ms: pinned.opened_at_unix_ms,
                    expires_at_unix_ms,
                    cf_name: cf_name.to_owned(),
                    physical_present: false,
                    logical_present: false,
                    expired_at_snapshot: false,
                    written_at_unix_ms: None,
                    retention_expires_at_unix_ms: None,
                    payload_len_bytes: None,
                    payload_sha256: None,
                });
            };
            if native_calyx_row {
                return Ok(native_snapshot_readback(
                    lease_id, pinned, current_seq, cf_name, &physical,
                ));
            }
            let envelope = decode_calyx_value_raw(&physical).map_err(|detail| {
                tracing::error!(
                    code = error_codes::STORAGE_READ_FAILED,
                    cf = cf_name,
                    lease_id,
                    snapshot_seq = pinned.snapshot.seq(),
                    detail,
                    "Calyx historical read rejected malformed KV retention envelope"
                );
                StorageError::ReadFailed {
                    cf_name: cf_name.to_owned(),
                    detail: format!(
                        "SYNAPSE_CALYX_SNAPSHOT_ROW_ENVELOPE_INVALID: lease_id={lease_id} snapshot_seq={} physical row does not decode: {detail}; remediation=preserve the vault and inspect the exact logical CF/key writer",
                        pinned.snapshot.seq()
                    ),
                }
            })?;
            let expired_at_snapshot = calyx_value_is_expired(
                envelope.expires_at_ms,
                pinned.opened_at_unix_ms,
            );
            Ok(CalyxStorageSnapshotReadback {
                lease_id,
                snapshot_seq: pinned.snapshot.seq(),
                current_seq,
                opened_at_unix_ms: pinned.opened_at_unix_ms,
                expires_at_unix_ms,
                cf_name: cf_name.to_owned(),
                physical_present: true,
                logical_present: !expired_at_snapshot,
                expired_at_snapshot,
                written_at_unix_ms: Some(envelope.written_at_ms),
                retention_expires_at_unix_ms: (envelope.expires_at_ms != 0)
                    .then_some(envelope.expires_at_ms),
                payload_len_bytes: Some(
                    u64::try_from(envelope.payload.len()).unwrap_or(u64::MAX),
                ),
                payload_sha256: Some(sha256_hex(envelope.payload)),
            })
        })
    }

    fn release_calyx_storage_snapshot(
        &self,
        lease_id: u64,
    ) -> StorageResult<CalyxStorageSnapshotRelease> {
        if lease_id == 0 {
            return Err(StorageError::ReadFailed {
                cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                detail: "SYNAPSE_CALYX_SNAPSHOT_LEASE_ID_INVALID: lease_id must be nonzero; remediation=pass the exact lease_id returned by snapshot_open".to_owned(),
            });
        }
        self.with_vault(
            "calyx_mvcc_snapshot_leases",
            "release Calyx MVCC storage snapshot",
            false,
            |vault| {
                let pinned = {
                    let mut snapshots = self.lock_storage_snapshots()?;
                    snapshots.remove(&lease_id).ok_or_else(|| StorageError::ReadFailed {
                        cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                        detail: format!(
                            "SYNAPSE_CALYX_SNAPSHOT_LEASE_UNKNOWN: lease_id={lease_id} is not active in this daemon; remediation=release each lease exactly once, before its expiry and without crossing a daemon restart"
                        ),
                    })?
                };
                let released = vault.release_reader(lease_id);
                if !released {
                    return Err(StorageError::ReadFailed {
                        cf_name: "calyx_mvcc_snapshot_leases".to_owned(),
                        detail: format!(
                            "SYNAPSE_CALYX_SNAPSHOT_LEASE_NOT_LIVE: lease_id={lease_id} existed in the public lease table but Aster no longer considered it live; remediation=open a new bounded snapshot and preserve daemon logs if this occurred before the reported expiry"
                        ),
                    });
                }
                // The admission invariant keeps this table at or below 64 rows.
                let active_lease_count = self.lock_storage_snapshots()?.len() as u64;
                Ok(CalyxStorageSnapshotRelease {
                    lease_id,
                    snapshot_seq: pinned.snapshot.seq(),
                    current_seq: vault.latest_seq(),
                    released,
                    active_lease_count,
                })
            },
        )
    }

    fn put_batch_if_revision_pressure_bypass(
        &self,
        cf_name: &str,
        guard_key: &[u8],
        expected_revision_sha256: Option<[u8; 32]>,
        rows: Vec<RawRow>,
    ) -> StorageResult<RevisionGuardedWriteOutcome> {
        let outcome = self.mutate_batch_if_revisions_pressure_bypass(
            cf_name,
            vec![RevisionGuard::new(guard_key, expected_revision_sha256)],
            Vec::new(),
            rows,
        )?;
        let previous_revision_sha256 = outcome
            .actual_revisions_sha256
            .first()
            .copied()
            .ok_or_else(|| {
                revision_guarded_mutation_failed(
                    cf_name,
                    STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
                    "single-key guarded mutation returned no actual guard revision".to_owned(),
                )
            })?;
        let committed_revision_sha256 = if outcome.applied {
            outcome
                .committed_revisions_sha256
                .first()
                .copied()
                .ok_or_else(|| {
                    revision_guarded_mutation_failed(
                        cf_name,
                        STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
                        "applied single-key guarded mutation returned no committed guard revision"
                            .to_owned(),
                    )
                })?
        } else {
            None
        };
        Ok(RevisionGuardedWriteOutcome {
            applied: outcome.applied,
            committed_seq: outcome.committed_seq,
            previous_revision_sha256,
            committed_revision_sha256,
        })
    }

    fn mutate_batch_if_revisions_pressure_bypass(
        &self,
        cf_name: &str,
        guards: Vec<RevisionGuard>,
        deletes: Vec<Vec<u8>>,
        puts: Vec<RawRow>,
    ) -> StorageResult<RevisionGuardedMutationOutcome> {
        validate_revision_guarded_mutation(cf_name, &guards, &deletes, &puts)?;
        let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
        self.with_vault(
            cf_name,
            "write multi-key revision-guarded Calyx KV mutation",
            true,
            |vault| {
                let now_ms = calyx_clock_now_for_write(vault, cf_name)?;
                let physical_guards = guards
                    .iter()
                    .map(|guard| {
                        encode_calyx_key_for_write(cf_name, collection_id, &guard.key).map(
                            |physical_key| {
                                SynapseCalyxRevisionGuard::new(
                                    ColumnFamily::Kv,
                                    physical_key,
                                    guard.expected_revision_sha256,
                                )
                            },
                        )
                    })
                    .collect::<StorageResult<Vec<_>>>()?;
                let mut writes = Vec::with_capacity(deletes.len().saturating_add(puts.len()));
                for key in &deletes {
                    writes.push(calyx_delete_row(cf_name, collection_id, key)?);
                }
                for (key, value) in &puts {
                    writes.push(calyx_put_row(cf_name, collection_id, key, value, now_ms)?);
                }
                commit_calyx_rows_if_revisions(vault, cf_name, &guards, &physical_guards, writes)
            },
        )
    }

    fn mutate_batch_pressure_bypass(
        &self,
        cf_name: &str,
        deletes: Vec<Vec<u8>>,
        puts: Vec<RawRow>,
    ) -> StorageResult<()> {
        let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
        self.with_vault(cf_name, "mutate Calyx KV batch", true, |vault| {
            let now_ms = calyx_clock_now_for_write(vault, cf_name)?;
            let put_count = puts.len();
            let mut rows = Vec::with_capacity(deletes.len().saturating_add(put_count));
            for key in deletes {
                rows.push(calyx_delete_row(cf_name, collection_id, &key)?);
            }
            for (key, value) in puts {
                rows.push(calyx_put_row(cf_name, collection_id, &key, &value, now_ms)?);
            }
            commit_calyx_rows_to_vault(vault, cf_name, rows)
        })
    }

    fn delete_batch(&self, cf_name: &str, keys: Vec<Vec<u8>>) -> StorageResult<()> {
        let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
        let rows = keys
            .into_iter()
            .map(|key| calyx_delete_row(cf_name, collection_id, &key))
            .collect::<StorageResult<Vec<_>>>()?;
        self.commit_rows(cf_name, rows)
    }

    fn flush(&self) -> StorageResult<()> {
        self.with_vault("<all>", "flush Calyx vault", true, |vault| {
            vault
                .flush()
                .map_err(|source| calyx_write_failed("<all>", "flush Calyx vault", &source))
        })
    }

    /// On-demand equivalent of one `storage_gc` tick.
    ///
    /// Runs the same two passes in the same order as the periodic tick,
    /// reclamation included (#2122) — an on-demand "run GC now" that quietly
    /// omitted half of what the scheduled one does would make the two
    /// indistinguishable in the readback and different in effect. The only
    /// explicit call joins the vault's single GC authority, so it observes the
    /// same exact delta baseline and memory-pressure state as the scheduled
    /// tick instead of allocating a second corpus-sized census.
    fn run_gc_once(&self) -> StorageResult<gc::GcReport> {
        self.gc_runner.run_full_once()
    }

    fn run_gc_once_with_row_caps(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<gc::GcReport> {
        self.gc_runner
            .run_full_once_with_row_cap(cf_name, soft_cap_rows, hard_cap_rows)
    }

    fn spawn_gc_task(&self) -> StorageResult<gc::GcTask> {
        let config = gc::GcConfig::from_retention_defaults();
        gc::spawn_runner(
            self.gc_runner.clone(),
            config.interval(),
            gc::MaintenanceTaskKind::GarbageCollection,
        )
    }

    fn spawn_checkpoint_task(&self) -> StorageResult<gc::GcTask> {
        gc::spawn_runner(
            Arc::new(CalyxCheckpointRunner::new(Arc::clone(&self.vault))),
            CALYX_CHECKPOINT_INTERVAL,
            gc::MaintenanceTaskKind::Checkpoint,
        )
    }

    fn spawn_derived_state_task(&self) -> StorageResult<gc::GcTask> {
        gc::spawn_runner(
            Arc::new(CalyxDerivedStateRunner::default()),
            crate::derived_state::DERIVED_STATE_INTERVAL,
            gc::MaintenanceTaskKind::DerivedState,
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "claim, source read, remeasurement, physical verification, and durable completion are one bounded backfill transaction"
    )]
    fn run_panel_backfill(
        &self,
        panel_version: u32,
        limit: usize,
        recover_in_flight: bool,
    ) -> StorageResult<PanelLifecycleBackfillReport> {
        if !self.pressure.permits_write("calyx_constellation") {
            return Err(StorageError::WriteShed {
                cf_name: "calyx_constellation".to_owned(),
                pressure_level: format!("{:?}", self.pressure.level()),
                rows: limit,
            });
        }
        let entry = constellations::panel_catalog_entry_for_version(panel_version)
            .filter(|entry| entry.panel_version == panel_version)
            .ok_or_else(|| StorageError::BackendInvalidConfig {
                value: panel_version.to_string(),
                detail:
                    "panel lifecycle backfill requires an exact active built-in panel generation"
                        .to_owned(),
            })?;
        let claim = self.with_vault(
            "calyx_registry",
            "claim durable Calyx panel backfill batch",
            true,
            |vault| {
                vault
                    .claim_panel_backfill(entry.panel_name, limit, recover_in_flight)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_registry",
                            "claim durable Calyx panel backfill batch",
                            &source,
                        )
                    })
            },
        )?;
        let claimed = claim.tasks.len() as u64;
        let mut verified_cx_ids = Vec::with_capacity(claim.tasks.len());
        let mut last_seq = claim.committed_seq;
        let mut last_hash = claim.value_sha256;
        let mut final_state = claim.state;
        let mut sources = Vec::with_capacity(claim.tasks.len());
        for task in claim.tasks {
            if !self.pressure.permits_write("calyx_constellation") {
                return Err(StorageError::WriteShed {
                    cf_name: "calyx_constellation".to_owned(),
                    pressure_level: format!("{:?}", self.pressure.level()),
                    rows: 1,
                });
            }
            let pointer = self
                .with_vault(
                    "calyx_base",
                    "read lifecycle backfill source pointer",
                    false,
                    |vault| {
                        vault
                            .read_base_source_pointer(task.cx_id)
                            .map_err(|source| {
                                calyx_write_failed(
                                    "calyx_base",
                                    "read lifecycle backfill source pointer",
                                    &source,
                                )
                            })
                    },
                )?
                .ok_or_else(|| StorageError::ReadFailed {
                    cf_name: "calyx_base".to_owned(),
                    detail: format!(
                        "lifecycle backfill source constellation {} is absent",
                        task.cx_id
                    ),
                })?;
            let source_cf = pointer.source_cf.ok_or_else(|| StorageError::ReadFailed {
                cf_name: "calyx_base".to_owned(),
                detail: format!(
                    "lifecycle backfill source {} has no source CF metadata",
                    task.cx_id
                ),
            })?;
            let source_key_hex =
                pointer
                    .source_key_hex
                    .ok_or_else(|| StorageError::ReadFailed {
                        cf_name: "calyx_base".to_owned(),
                        detail: format!(
                            "lifecycle backfill source {} has no source key metadata",
                            task.cx_id
                        ),
                    })?;
            let source_key = decode_source_key_hex(&source_key_hex).map_err(|detail| {
                StorageError::ReadFailed {
                    cf_name: source_cf.clone(),
                    detail: format!("decode lifecycle source key {source_key_hex:?}: {detail}"),
                }
            })?;
            let raw =
                self.get_cf(&source_cf, &source_key)?
                    .ok_or_else(|| StorageError::ReadFailed {
                        cf_name: source_cf.clone(),
                        detail: format!(
                            "authoritative lifecycle source row key_hex={source_key_hex} is absent"
                        ),
                    })?;
            sources.push((task, source_cf, source_key, raw));
        }
        let materialized = if sources.is_empty() {
            Vec::new()
        } else {
            self.with_vault(
                "calyx_constellation",
                "batch-remeasure lifecycle backfill source rows",
                true,
                |vault| {
                    // #2062 ask 2. `panel_version` reaches this write as data
                    // read out of a catalog entry, not as a constant this
                    // process proved at vault open, so the claim is verified
                    // before the row is written rather than discovered by a
                    // census afterwards. Memoized once per generation.
                    ensure_base_write_generation_claimed(vault, panel_version)?;
                    let bases = sources
                        .iter()
                        .map(|(_task, source_cf, source_key, raw)| {
                            lifecycle_base_constellation_from_source(
                                vault,
                                entry.panel_name,
                                panel_version,
                                source_cf,
                                source_key,
                                raw,
                            )
                        })
                        .collect::<StorageResult<Vec<_>>>()?;
                    let lifecycle_rows = bases
                        .iter()
                        .zip(&sources)
                        .map(|((identity, base), (_task, _source_cf, _source_key, raw))| {
                            (identity.as_slice(), raw.as_slice(), base)
                        })
                        .collect::<Vec<_>>();
                    let latest = vault
                        .materialize_panel_lifecycle_generation_batch(
                            entry.panel_name,
                            &lifecycle_rows,
                        )
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "batch materialize lifecycle backfill generations",
                                &source,
                            )
                        })?;
                    if latest.len() != bases.len() {
                        return Err(calyx_write_failed_detail(
                            "calyx_constellation",
                            format!(
                                "lifecycle materialization returned {} rows for {} claimed sources",
                                latest.len(),
                                bases.len()
                            ),
                        ));
                    }
                    let expected = latest
                        .into_iter()
                        .enumerate()
                        .map(|(row_index, row)| {
                            row.ok_or_else(|| {
                                calyx_write_failed_detail(
                                    "calyx_registry",
                                    format!(
                                        "panel {} lifecycle row disappeared after batch claim at row_index={row_index}",
                                        entry.panel_name
                                    ),
                                )
                            })
                        })
                        .collect::<StorageResult<Vec<_>>>()?;
                    let puts = vault
                        .put_observation_constellation_batch(expected.clone())
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put lifecycle backfill constellation batch",
                                &source,
                            )
                        })?;
                    if puts.len() != expected.len() {
                        return Err(calyx_write_failed_detail(
                            "calyx_constellation",
                            format!(
                                "lifecycle constellation batch returned {} readbacks for {} rows",
                                puts.len(),
                                expected.len()
                            ),
                        ));
                    }
                    for (row_index, (put, expected)) in puts.iter().zip(&expected).enumerate() {
                        if put.cx_id != expected.cx_id.to_string() {
                            return Err(calyx_write_failed_detail(
                                "calyx_constellation",
                                format!(
                                    "lifecycle batch identity mismatch at row_index={row_index}: expected={} actual={}",
                                    expected.cx_id, put.cx_id
                                ),
                            ));
                        }
                        let observed = vault
                            .hydrate_constellation_latest(expected.cx_id)
                            .map_err(|source| {
                                calyx_write_failed(
                                    "calyx_constellation",
                                    "independently hydrate lifecycle backfill constellation batch row",
                                    &source,
                                )
                            })?;
                        if observed.cx_id != expected.cx_id
                            || observed.panel_version != expected.panel_version
                            || observed.slots != expected.slots
                            || observed.input_ref != expected.input_ref
                        {
                            return Err(calyx_write_failed_detail(
                                "calyx_constellation",
                                format!(
                                    "physical lifecycle batch readback differs at row_index={row_index} expected_cx_id={} panel={}",
                                    expected.cx_id, expected.panel_version
                                ),
                            ));
                        }
                    }
                    Ok(expected
                        .into_iter()
                        .map(|constellation| constellation.cx_id)
                        .collect::<Vec<_>>())
                },
            )?
        };
        if materialized.len() != sources.len() {
            return Err(calyx_write_failed_detail(
                "calyx_constellation",
                format!(
                    "lifecycle batch produced {} verified identities for {} claimed tasks",
                    materialized.len(),
                    sources.len()
                ),
            ));
        }
        for ((task, _source_cf, _source_key, _raw), materialized) in
            sources.into_iter().zip(materialized)
        {
            let completion = self.with_vault(
                "calyx_registry",
                "complete verified Calyx panel backfill task",
                true,
                |vault| {
                    vault
                        .complete_panel_backfill_task(entry.panel_name, task.id)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_registry",
                                "complete verified Calyx panel backfill task",
                                &source,
                            )
                        })
                },
            )?;
            verified_cx_ids.push(materialized.to_string());
            last_seq = completion.committed_seq;
            last_hash = completion.value_sha256;
            final_state = completion.state;
        }
        let queue = final_state.controller.queue();
        let pending = queue
            .tasks()
            .filter(|task| task.state == calyx_registry::BackfillState::Pending)
            .count() as u64;
        let in_flight = queue
            .tasks()
            .filter(|task| task.state == calyx_registry::BackfillState::InFlight)
            .count() as u64;
        let completed_total = queue.completed_len() as u64;
        Ok(PanelLifecycleBackfillReport {
            panel_name: entry.panel_name.to_owned(),
            source_panel_version: panel_version,
            target_panel_version: final_state.controller.panel().version,
            claimed,
            recovered_in_flight: claim.recovered_in_flight,
            completed: verified_cx_ids.len() as u64,
            pending,
            in_flight,
            completed_total,
            registry_committed_seq: last_seq,
            registry_value_sha256: last_hash,
            verified_cx_ids,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one sweep over one discovered generation set; the discovery, the per-generation disposition and the accounting are a single policy, and splitting them is what let the set being maintained drift away from the set that exists"
    )]
    fn maintain_calyx_search_generation(
        &self,
    ) -> StorageResult<crate::search_sweep::SearchGenerationSweep> {
        use crate::search_sweep::{
            GenerationDisposition, PanelGenerationMaintenance, SearchGenerationSweep,
        };

        self.vault.with_vault(
            "calyx_search_generation",
            "maintain every published Calyx search generation",
            true,
            |vault| {
                let started = std::time::Instant::now();
                // The set owed maintenance is what is *published on disk*, not
                // what the manifest points at (#1938). Reading the active-panel
                // pointer to answer "which generations exist" is what left every
                // non-active generation with no maintainer.
                let published = vault.published_search_generations().map_err(|source| {
                    calyx_write_failed(
                        "calyx_search_generation",
                        "enumerate the published Calyx search generations",
                        &source,
                    )
                })?;

                // The active panel is swept even when it has no generation yet,
                // because that is the InitialBuild case: a panel that has never
                // been built has no directory to be discovered by.
                let active_panel_version = vault.active_panel_version().map_err(|source| {
                    calyx_write_failed(
                        "calyx_search_generation",
                        "read the active durable panel for the search-generation sweep",
                        &source,
                    )
                })?;
                let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
                // #2075: discovering the set from disk fixed maintenance and
                // left bootstrap broken. A generation that has never been built
                // has no directory to be discovered by, and the only exemption
                // was the active panel — so the active panel could be born and
                // no other panel could. A panel version bump produces exactly
                // that state, and on the live vault it left the three
                // outcome-bearing corpora (episode, agent-transcript,
                // mcp-usage) with no persisted generation at all, so every
                // fused query naming them failed closed with
                // SYNAPSE_CALYX_FIND_INDEX_STALE while health reported search
                // `ok` from the active generation alone.
                //
                // "Which generations may be queried" is its own question and is
                // now asked directly, of the same declaration `find` fails
                // closed against.
                let declared_queryable =
                    constellations::declared_queryable_panel_versions(created_at_ms);
                let newest_closed_predecessor: std::collections::BTreeMap<&'static str, u32> =
                    published
                        .panels
                        .iter()
                        .filter_map(|version| {
                            superseded_panel_lineage(*version)
                                .map(|lineage| (lineage.panel_name, *version))
                        })
                        .fold(
                            std::collections::BTreeMap::new(),
                            |mut newest, (name, version)| {
                                newest
                                    .entry(name)
                                    .and_modify(|current| *current = (*current).max(version))
                                    .or_insert(version);
                                newest
                            },
                        );
                let mut targets = published.panels.clone();
                if let Some(active) = active_panel_version
                    && !targets.contains(&active)
                {
                    targets.push(active);
                }
                targets.extend(declared_queryable.iter().copied());
                targets.sort_unstable();
                targets.dedup();
                // The active generation is swept **first**, not in version
                // order. Per-panel isolation below already stops one panel's
                // failure from costing another its maintenance; this is the
                // second line of the same defence — if anything ever aborts the
                // pass mid-loop, the one generation every recall path depends on
                // has already been maintained rather than being whichever panel
                // happened to sort last (#1971 finding 2).
                if let Some(active) = active_panel_version
                    && let Some(position) = targets.iter().position(|version| *version == active)
                {
                    targets[..=position].rotate_right(1);
                }

                // Genuinely pass-level, so it is checked once here rather than
                // once per panel inside `syn_queryable_panel_contract`. An
                // undeclared lens provenance is a property of the *code*, not of
                // any one generation, and attributing it to whichever panel the
                // loop happened to reach first would name the wrong culprit
                // (#1971 finding 2).
                assert_syn_lens_provenance_complete()?;
                let mut generations = Vec::with_capacity(targets.len());
                for panel_version in targets {
                    let is_active_panel = active_panel_version == Some(panel_version);
                    let is_declared_queryable = declared_queryable.contains(&panel_version);
                    // The pre-pass disk census, captured per target so the
                    // post-pass manifest presence is defined for dispositions
                    // that produce no report (#2075).
                    let manifest_present_at_start = published.panels.contains(&panel_version);
                    // A non-active generation can only be rebuilt from its own
                    // code-declared contract. Resolving it here, before the
                    // attempt, turns "no contract" from a rebuild failure into a
                    // named terminal state (#1938 ask 2).
                    //
                    // Building the contract can itself *fail* — an admission
                    // gate such as CALYX_PANEL_SLOT_COSINE_CONSTANT fires right
                    // here. That failure belongs to the one panel that produced
                    // it. Propagating it with `?` aborted the entire sweep, so
                    // one legacy panel took the *active* panel's generation down
                    // with it and recall went to zero (#1971 finding 2). It is
                    // now recorded against its own generation exactly like a
                    // rebuild failure, and the sweep continues.
                    let supplied = match syn_queryable_panel_contract(panel_version, created_at_ms)
                    {
                        Ok(contract) => contract.map(|contract| SynapseCalyxPanelState {
                            panel: contract.panel,
                            registry: contract.registry,
                            registry_snapshot: None,
                        }),
                        Err(error) => {
                            tracing::error!(
                                code = "STORAGE_SEARCH_GENERATION_CONTRACT_BUILD_FAILED",
                                panel_version,
                                is_active_panel,
                                detail = %error,
                                "building one panel's code-declared slot contract failed, so that \
                                 generation cannot be maintained; the sweep continues with the \
                                 remaining generations and the pass is recorded as failed"
                            );
                            generations.push(PanelGenerationMaintenance {
                                panel_version,
                                is_active_panel,
                                is_declared_queryable,
                                manifest_present_at_start,
                                disposition: GenerationDisposition::Failed {
                                    code: "STORAGE_SEARCH_GENERATION_CONTRACT_BUILD_FAILED"
                                        .to_owned(),
                                    detail: format!(
                                        "build the code-declared slot contract for panel \
                                         {panel_version}: {error}"
                                    ),
                                },
                            });
                            continue;
                        }
                    };
                    if supplied.is_none() && !is_active_panel {
                        // #1972 ask 2: "no contract" collapsed two situations
                        // with opposite remedies. A closed superseded version of
                        // a live panel is reclaimable and its removal is safe;
                        // a version nothing declares must be investigated before
                        // anything is deleted. The catalog already knows which
                        // is which, so this is a lookup rather than a judgement.
                        // #2263 ask 1: "no contract" collapsed a *third*
                        // situation. The live versions of the finite-only
                        // panels — agent-event, reflex, process, observation —
                        // have no query contract because
                        // `syn_panel_is_queryable` deliberately excludes them
                        // (#1965 retired their graded dense lens), and they are
                        // live, so `superseded_panel_lineage` does not know
                        // them either. They fell to UnmaintainableNoContract,
                        // whose text asserts the version has no place in any
                        // live panel's lineage and tells an operator to
                        // investigate what wrote it — two false claims about
                        // the four panels the catalog declares. Checked before
                        // the lineage lookup because being the live version is
                        // the stronger fact.
                        let live_finite_only = constellations::panel_catalog_entry_for_version(
                            panel_version,
                        )
                        .filter(|entry| entry.panel_version == panel_version)
                        .map(|entry| entry.panel_name);
                        let disposition = if let Some(panel_name) = live_finite_only {
                            tracing::debug!(
                                code = "STORAGE_SEARCH_GENERATION_FINITE_ONLY_LIVE_PANEL",
                                panel_version,
                                panel_name,
                                index_root = %published.index_root.display(),
                                "a search generation is published for the live version of a \
                                 finite-only panel that the code deliberately does not admit to \
                                 fused search; this is a declared terminal state, not damage"
                            );
                            GenerationDisposition::FiniteOnlyLivePanel { panel_name }
                        } else if let Some(lineage) =
                            superseded_panel_lineage(panel_version)
                        {
                            let retain_for_rollback = newest_closed_predecessor
                                .get(lineage.panel_name)
                                .is_some_and(|newest| *newest == panel_version);
                            if retain_for_rollback {
                                tracing::info!(
                                code = "STORAGE_SEARCH_GENERATION_RETIRABLE_SUPERSEDED",
                                panel_version,
                                panel_name = lineage.panel_name,
                                live_panel_version = lineage.live_panel_version,
                                index_root = %published.index_root.display(),
                                "a search generation is published for a closed superseded \
                                 version of a live panel; the live generation carries the \
                                 corpus, so this directory is reclaimable through storage \
                                 operation=retire_search_generation"
                                );
                                GenerationDisposition::RetirableSupersededGeneration {
                                    panel_name: lineage.panel_name,
                                    live_panel_version: lineage.live_panel_version,
                                }
                            } else {
                                match vault.retire_search_generation(panel_version) {
                                    Ok(_) => GenerationDisposition::RetiredSupersededGeneration {
                                        panel_name: lineage.panel_name,
                                        live_panel_version: lineage.live_panel_version,
                                    },
                                    Err(source) => GenerationDisposition::Failed {
                                        code: source.code.to_string(),
                                        detail: format!(
                                            "{}: {}",
                                            source.message, source.remediation
                                        ),
                                    },
                                }
                            }
                        } else {
                            tracing::warn!(
                                code = "STORAGE_SEARCH_GENERATION_UNMAINTAINABLE_NO_CONTRACT",
                                panel_version,
                                index_root = %published.index_root.display(),
                                "a search generation is published for a panel version with \
                                 no code-declared slot contract and no place in any live \
                                 panel's declared lineage; nothing can rebuild it, no query \
                                 can measure through it, and nothing establishes what it is. \
                                 Investigate before deleting anything"
                            );
                            GenerationDisposition::UnmaintainableNoContract
                        };
                        generations.push(PanelGenerationMaintenance {
                            panel_version,
                            is_active_panel,
                            is_declared_queryable,
                            manifest_present_at_start,
                            disposition,
                        });
                        continue;
                    }
                    // One generation's failure must not cost every other
                    // generation its maintenance — but it is still a failure of
                    // the pass, recorded against the exact panel that produced
                    // it rather than collapsed into a single pass-level error.
                    let disposition = match vault
                        .maintain_search_generation_for_panel(panel_version, supplied.as_ref())
                    {
                        Ok(report) => GenerationDisposition::Maintained(Box::new(report)),
                        Err(source) => {
                            tracing::error!(
                                code = "STORAGE_SEARCH_GENERATION_MAINTENANCE_FAILED",
                                panel_version,
                                is_active_panel,
                                failure_code = %source.code,
                                detail = %source.message,
                                remediation = %source.remediation,
                                "maintaining one published search generation failed; the sweep \
                                 continues with the remaining generations and the pass is \
                                 recorded as failed"
                            );
                            GenerationDisposition::Failed {
                                code: source.code.to_string(),
                                detail: format!("{}: {}", source.message, source.remediation),
                            }
                        }
                    };
                    generations.push(PanelGenerationMaintenance {
                        panel_version,
                        is_active_panel,
                        is_declared_queryable,
                        manifest_present_at_start,
                        disposition,
                    });
                }

                let last_rebuild_memory = generations.iter().rev().find_map(|entry| {
                    entry.rebuild_private_bytes().map(|(before, peak, after)| {
                        crate::search_sweep::SearchRebuildMemoryObservation {
                            panel_version: entry.panel_version,
                            private_bytes_before: before,
                            private_bytes_peak: peak,
                            private_bytes_after: after,
                        }
                    })
                });
                let sweep = SearchGenerationSweep {
                    index_root: published.index_root.display().to_string(),
                    active_panel_version,
                    declared_queryable_panel_versions: declared_queryable,
                    generations,
                    unrecognized_index_entries: published.unrecognized,
                    last_rebuild_memory,
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                };
                // #2075: named loudly, at the pass that owns the condition. A
                // declared-queryable panel with no manifest is not a pending
                // chore — every fused query naming it is failing closed right
                // now, and the active generation's health says nothing about it.
                let unbuilt = sweep.unbuilt_declared_queryable_panel_versions();
                if !unbuilt.is_empty() {
                    tracing::warn!(
                        code = "STORAGE_SEARCH_GENERATION_DECLARED_QUERYABLE_UNBUILT",
                        unbuilt_declared_queryable_panels = ?unbuilt,
                        declared_queryable_panels = ?sweep.declared_queryable_panel_versions,
                        index_root = %sweep.index_root,
                        detail = %sweep.summary_line(),
                        "one or more panel generations the tool contract declares queryable have \
                         no persisted search generation, so every fused find naming them fails \
                         closed with SYNAPSE_CALYX_FIND_INDEX_STALE; the sweep enrolled them for \
                         an initial build, and a version still listed here after the next pass \
                         means that build did not complete"
                    );
                }
                tracing::info!(
                    code = "STORAGE_SEARCH_GENERATION_SWEEP_COMPLETED",
                    unbuilt_declared_queryable = unbuilt.len(),
                    generations = sweep.generations.len(),
                    any_failed = sweep.any_failed(),
                    closest_to_bound = ?sweep.closest_to_bound(),
                    elapsed_ms = sweep.elapsed_ms,
                    detail = %sweep.summary_line(),
                    "swept every published Calyx search generation"
                );
                Ok(sweep)
            },
        )
    }

    fn measure_calyx_lens_coverage(
        &self,
        max_records: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxLensCoverageStatus> {
        // Association health follows query admission rather than the storage
        // schema catalog. Every panel here has a sealed search-membership
        // generation and graded geometry; reconstructable finite-only panels
        // (notably agent-event after #1965) are intentionally absent.
        let panels = constellations::declared_queryable_panel_versions(0);
        self.vault.with_vault(
            "calyx_lens_coverage",
            "measure Calyx panel lens coverage",
            true,
            |vault| {
                vault
                    .lens_coverage_status(&panels, max_records)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_lens_coverage",
                            "measure Calyx panel lens coverage",
                            &source,
                        )
                    })
            },
        )
    }

    fn measure_panel_coverage(&self) -> StorageResult<crate::panel_coverage::PanelCoverageReport> {
        // #2243: index each declared physical source namespace BEFORE decoding
        // Base. The old order retained ~1.9 million hex Strings from Base and
        // then joined them against source rows, duplicating the corpus and
        // pushing a background-only census above 1 GiB. This index stores each
        // physical key once as contiguous raw bytes plus one u32 end offset;
        // Base can therefore perform exact membership as each row passes and
        // retain only the rare keys proven absent.
        let declared_source_cfs = panel_coverage_declared_source_cfs();
        let (census, source_indices, source_cf_rows) = self.vault.with_vault(
            "calyx_panel_coverage",
            "measure source membership and every Base generation at one pinned Calyx sequence",
            true,
            |vault| {
                let reader = CalyxPinnedReader::pin(
                    vault,
                    "calyx_panel_coverage",
                    PANEL_COVERAGE_SOURCE_CENSUS_SITE,
                    CALYX_GC_SOURCE_CENSUS_LEASE_MS,
                )?;
                let pinned_seq = reader.pinned_seq();
                let read_at_unix_ms = vault.clock_now_ms().map_err(|source| {
                    calyx_read_failed(
                        "calyx_panel_coverage",
                        "read the vault clock for the pinned panel-coverage census",
                        &source,
                    )
                })?;
                let mut source_indices: BTreeMap<String, PackedSortedKeys> = BTreeMap::new();
                let mut source_cf_rows = BTreeMap::new();
                for (cf_name, source_is_full_cf) in declared_source_cfs {
                    // Membership is measured for sampled and full-CF panels:
                    // both can have orphans. Expired rows remain physically
                    // present in the index, but only live rows contribute to a
                    // full-CF coverage denominator (#1882/#1940). The shared pin
                    // excludes later commits from every source and Base page.
                    let (live_rows, index) = build_panel_coverage_source_index(
                        &reader,
                        &cf_name,
                        read_at_unix_ms,
                    )?;
                    tracing::info!(
                        code = "SYNAPSE_PANEL_COVERAGE_SOURCE_INDEX_COMPLETE",
                        source_cf = %cf_name,
                        pinned_seq,
                        physical_key_count = index.len(),
                        packed_key_bytes = index.packed_bytes(),
                        offset_bytes = index.offset_bytes(),
                        live_rows,
                        "panel coverage built an exact allocation-compact physical source-key index"
                    );
                    // A subset-fed panel has no meaningful coverage ratio.
                    if source_is_full_cf {
                        source_cf_rows.insert(cf_name.clone(), live_rows);
                    }
                    source_indices.insert(cf_name, index);
                }

                let census = vault
                    .panel_census_snapshot(reader.snapshot(), |source_cf, source_key_hex| {
                        source_indices
                            .get(source_cf)
                            .map(|index| index.contains_lower_hex(source_key_hex))
                    })
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_panel_coverage",
                            "census every Calyx panel generation with exact source membership at the pinned sequence",
                            &source,
                        )
                    })?;
                tracing::info!(
                    code = "SYNAPSE_PANEL_COVERAGE_PINNED_CENSUS_COMPLETE",
                    pinned_seq,
                    source_cf_count = source_indices.len(),
                    base_cf_rows = census.base_cf_rows,
                    decode_failures = census.decode_failures,
                    "panel coverage completed source and Base membership at one committed Calyx sequence"
                );
                Ok((census, source_indices, source_cf_rows))
            },
        )?;

        // #2062: the second authority. The catalog is a compile-time table and
        // structurally cannot claim a generation minted at runtime, so a census
        // joined against it alone reported 98 owned generations holding 62,566
        // rows as claimed by nothing and held `calyx_panel_coverage` at `error`
        // on that basis. This read is one Registry-CF point read per five-minute
        // tick, against a whole-Base scan it sits beside.
        let allocator = self.panel_generation_allocator()?;
        let ownership = crate::panel_coverage::PanelGenerationOwnership {
            owners: allocator.owners,
            retired: allocator.retired,
        };

        let report = crate::panel_coverage::build_panel_coverage_report(
            &census,
            &source_cf_rows,
            &|source_cf, source_key_hex| {
                source_indices
                    .get(source_cf)
                    .map(|index| index.contains_lower_hex(source_key_hex))
            },
            &ownership,
        );
        if !report.accounting_holds() {
            // Never a silent discrepancy: the caller is told the counts do not
            // add up, with all three numbers, rather than being handed a report
            // whose denominators cannot be trusted.
            return Err(StorageError::ReadFailed {
                cf_name: "calyx_panel_census".to_owned(),
                detail: format!(
                    "SYNAPSE_PANEL_CENSUS_ACCOUNTING_MISMATCH: base_cf_rows={} != records_total={} \
                     + decode_failures={}; every Base row must be counted into exactly one panel \
                     generation or into the decode-failure count",
                    report.base_cf_rows, report.records_total, report.decode_failures
                ),
            });
        }
        Ok(report)
    }

    fn timeseries_write(
        &self,
        collection_name: &str,
        series: u64,
        timestamp_ns: u64,
        value: f64,
    ) -> StorageResult<u64> {
        self.with_vault(
            "calyx_timeseries",
            "write native TimeSeries point",
            true,
            |vault| {
                vault
                    .timeseries_write(collection_name, series, timestamp_ns, value)
                    .map_err(|error| {
                        calyx_write_failed(
                            "calyx_timeseries",
                            "write native TimeSeries point and continuous rollups",
                            &error,
                        )
                    })
            },
        )
    }

    fn olap_aggregate_slot(
        &self,
        panel_version: u32,
        slot_id: u32,
        value_column: usize,
        group_by_column: Option<usize>,
        max_rows: usize,
        max_groups: usize,
    ) -> StorageResult<synapse_calyx::olap::OlapScanResult> {
        self.with_vault(
            "calyx_olap",
            "scan native OLAP slot column",
            false,
            |vault| {
                let created_at_ms = calyx_clock_now_for_read(vault, "calyx_registry")?;
                let panel = resolve_queryable_panel_contract(vault, panel_version, created_at_ms)?
                    .ok_or_else(|| StorageError::BackendInvalidConfig {
                        value: panel_version.to_string(),
                        detail: format!(
                            "SYNAPSE_CALYX_OLAP_PANEL_UNKNOWN: panel {panel_version} has no declared Registry contract; remediation=inspect the panel catalog and pass an exact declared panel_version"
                        ),
                    })?;
                if !panel
                    .panel
                    .slots
                    .iter()
                    .any(|slot| u32::from(slot.slot_id.0) == slot_id)
                {
                    let declared_slot_ids = panel
                        .panel
                        .slots
                        .iter()
                        .map(|slot| slot.slot_id.0.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    return Err(StorageError::BackendInvalidConfig {
                        value: slot_id.to_string(),
                        detail: format!(
                            "SYNAPSE_CALYX_OLAP_SLOT_UNDECLARED: slot {slot_id} is not declared by Registry panel {panel_version}; declared_slot_ids=[{declared_slot_ids}]; remediation=pass one of the exact declared slot ids for this panel"
                        ),
                    });
                }
                vault
                    .olap_aggregate_slot(
                        panel_version,
                        slot_id,
                        value_column,
                        group_by_column,
                        max_rows,
                        max_groups,
                    )
                    .map_err(|error| {
                        calyx_read_failed(
                            "calyx_olap",
                            "materialize and scan native OLAP slot column",
                            &error,
                        )
                    })
            },
        )
    }

    fn ensure_timeseries_collection(&self, collection_name: &str) -> StorageResult<()> {
        self.with_vault(
            "calyx_timeseries",
            "ensure native TimeSeries collection",
            true,
            |vault| {
                vault
                    .ensure_timeseries_collection(collection_name)
                    .map_err(|error| {
                        calyx_write_failed(
                            "calyx_timeseries",
                            "create and read back native TimeSeries collection descriptor",
                            &error,
                        )
                    })
            },
        )
    }

    fn timeseries_rollup(
        &self,
        collection_name: &str,
        series: u64,
        window: synapse_calyx::timeseries::SynapseCalyxRollupWindow,
        timestamp_ns: u64,
    ) -> StorageResult<Option<synapse_calyx::timeseries::SynapseCalyxRollupValue>> {
        self.with_vault(
            "calyx_timeseries",
            "read native TimeSeries rollup",
            false,
            |vault| {
                vault
                    .timeseries_rollup(collection_name, series, window, timestamp_ns)
                    .map_err(|error| {
                        calyx_read_failed(
                            "calyx_timeseries",
                            "read native TimeSeries rollup",
                            &error,
                        )
                    })
            },
        )
    }

    fn timeseries_range(
        &self,
        collection_name: &str,
        series: u64,
        start_timestamp_ns: u64,
        end_timestamp_ns: u64,
    ) -> StorageResult<Vec<(u64, f64)>> {
        self.with_vault(
            "calyx_timeseries",
            "read native TimeSeries range",
            false,
            |vault| {
                vault
                    .timeseries_range(
                        collection_name,
                        series,
                        start_timestamp_ns,
                        end_timestamp_ns,
                    )
                    .map_err(|error| {
                        calyx_read_failed(
                            "calyx_timeseries",
                            "read native TimeSeries point range",
                            &error,
                        )
                    })
            },
        )
    }

    fn pressure_level(&self) -> pressure::DiskPressureLevel {
        self.pressure.level()
    }

    fn pressure_permits_write(&self, cf_name: &str) -> bool {
        self.pressure.permits_write(cf_name)
    }

    fn pressure_transition_codes(&self) -> StorageResult<Vec<&'static str>> {
        self.pressure.transition_codes()
    }

    fn pressure_probe_readback(&self) -> StorageResult<pressure::PressureProbeReadback> {
        self.pressure.probe_readback()
    }

    /// Logical bytes per column family, folded with **bounded** row-guard holds.
    ///
    /// Identical arithmetic to the materialising version it replaces — the same
    /// decoded logical key plus payload of the same live rows — but the rows are
    /// summed page by page and never all held at once, and the MVCC row-table
    /// read guard is released between pages (#2041). The previous shape took one
    /// guard acquisition per column family and held it for the whole family;
    /// with `storage inspect` also calling `cf_row_counts` and `scan_cf_tail`
    /// that was 51 unbounded holds of the lock every vault write needs.
    fn cf_sizes(&self) -> StorageResult<BTreeMap<String, u64>> {
        let mut sizes: BTreeMap<String, u64> = cf::ALL_COLUMN_FAMILIES
            .iter()
            .map(|cf_name| ((*cf_name).to_owned(), 0_u64))
            .collect();
        self.with_vault(
            "<calyx-vault>",
            "sum logical Calyx namespace bytes with bounded row-guard holds",
            false,
            |vault| {
                sweep_every_calyx_namespace(vault, "cf_sizes", |cf_name, key, payload| {
                    let entry = sizes.entry(cf_name.to_owned()).or_insert(0);
                    *entry = entry
                        .saturating_add(key.len() as u64)
                        .saturating_add(payload.len() as u64);
                    Ok(())
                })
            },
        )?;
        emit_storage_cf_bytes(&sizes);
        Ok(sizes)
    }

    fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap> {
        let sizes = self.cf_sizes()?;
        Ok((sizes, Vec::new()))
    }

    /// Live rows per column family, counted with **bounded** row-guard holds.
    ///
    /// The count is the same set of rows the materialising version counted —
    /// same decode, same expiry filter, same strictly-increasing-key assertion —
    /// but nothing is retained beyond the counter, so a 200,000-row family costs
    /// one `u64` instead of a vector of its whole contents, and the row-table
    /// read guard is released every 256 candidates (#2041).
    fn cf_row_counts(&self) -> StorageResult<BTreeMap<String, u64>> {
        let mut counts: BTreeMap<String, u64> = cf::ALL_COLUMN_FAMILIES
            .iter()
            .map(|cf_name| ((*cf_name).to_owned(), 0_u64))
            .collect();
        self.with_vault(
            "<calyx-vault>",
            "count live Calyx namespace rows with bounded row-guard holds",
            false,
            |vault| {
                sweep_every_calyx_namespace(vault, "cf_row_counts", |cf_name, _key, _payload| {
                    let entry = counts.entry(cf_name.to_owned()).or_insert(0);
                    *entry = entry.saturating_add(1);
                    Ok(())
                })
            },
        )?;
        Ok(counts)
    }

    fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap> {
        let counts = self.cf_row_counts()?;
        Ok((counts, Vec::new()))
    }

    fn snapshot_gc_observation(&self) -> StorageResult<SynapseCalyxSnapshotGcObservation> {
        self.with_vault(
            "<calyx-vault>",
            "read physical snapshot-version GC counters",
            false,
            |vault| Ok(vault.snapshot_gc_observation()),
        )
    }

    fn calyx_vault_status(&self) -> StorageResult<SynapseCalyxVaultStatus> {
        self.vault.status()
    }

    fn calyx_search_generation_status(
        &self,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchGenerationStatus> {
        self.with_vault(
            "<calyx-vault>",
            "read Calyx persisted search generation status",
            false,
            |vault| {
                vault.search_generation_status().map_err(|source| {
                    calyx_write_failed(
                        "<calyx-vault>",
                        "read Calyx persisted search generation status",
                        &source,
                    )
                })
            },
        )
    }

    fn calyx_search_generation_status_for_panel(
        &self,
        panel_version: u32,
        measure_delta: bool,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchGenerationStatus> {
        self.with_vault(
            "<calyx-vault>",
            "read one panel's Calyx persisted search generation status",
            false,
            |vault| {
                vault
                    .search_generation_status_for_panel(panel_version, measure_delta)
                    .map_err(|source| {
                        calyx_write_failed(
                            "<calyx-vault>",
                            "read one panel's Calyx persisted search generation status",
                            &source,
                        )
                    })
            },
        )
    }

    fn diagnose_constellation_row_sequences(
        &self,
        cx_id: calyx_core::CxId,
    ) -> StorageResult<synapse_calyx::ConstellationRowSequences> {
        self.with_vault(
            "<calyx-vault>",
            "read one constellation's Base and slot row MVCC sequences",
            false,
            |vault| {
                vault
                    .diagnose_constellation_row_sequences(cx_id)
                    .map_err(|source| {
                        calyx_write_failed(
                            "<calyx-vault>",
                            "read one constellation's Base and slot row MVCC sequences",
                            &source,
                        )
                    })
            },
        )
    }

    fn calyx_changed_key_count_after(&self, cf_name: &str, after_seq: u64) -> StorageResult<u64> {
        self.with_vault(
            "<calyx-vault>",
            "count a native CF's MVCC changed keys",
            false,
            |vault| {
                vault
                    .changed_key_count_after(cf_name, after_seq)
                    .map_err(|source| {
                        calyx_write_failed(
                            "<calyx-vault>",
                            "count a native CF's MVCC changed keys",
                            &source,
                        )
                    })
            },
        )
    }

    fn calyx_cf_row_count(&self, cf_name: &str) -> StorageResult<u64> {
        self.with_vault(
            "<calyx-vault>",
            "count a native Calyx CF's rows",
            false,
            |vault| {
                vault.cf_row_count(cf_name).map_err(|source| {
                    calyx_write_failed("<calyx-vault>", "count a native Calyx CF's rows", &source)
                })
            },
        )
    }

    fn lower_guard_thresholds(
        &self,
        params: &synapse_calyx::LoweringParams,
    ) -> StorageResult<synapse_calyx::LoweredPublishReport> {
        self.with_vault(
            "<calyx-vault>",
            "lower Calyx guard thresholds to the frozen hot-path artifact",
            false,
            |vault| {
                vault.lower_guard_thresholds(params).map_err(|source| {
                    calyx_write_failed(
                        "<calyx-vault>",
                        "lower Calyx guard thresholds to the frozen hot-path artifact",
                        &source,
                    )
                })
            },
        )
    }

    fn rebuild_calyx_search_indexes(
        &self,
        expected_panel_version: u32,
    ) -> StorageResult<SynapseCalyxSearchRebuildReport> {
        self.with_vault(
            "calyx_search",
            "rebuild persisted Calyx search indexes",
            true,
            |vault| {
                // The generation must be rooted at the requested panel's
                // authoritative snapshot (#1805). It used to get there by
                // *publishing* that panel as the vault's active one first,
                // because the manifest holds a single `panel_ref` and that
                // pointer was the only way to tell the rebuild which slot map to
                // use. That made the active-panel pointer a scratch variable: a
                // rebuild of any non-timeline panel deposed timeline, and the
                // maintainer's next tick swapped it back (#1668).
                //
                // The contract is now handed to the rebuild directly, so the
                // generation is rooted at exactly the same snapshot with no
                // manifest mutation at all. Boot publishes the active panel
                // (see `publish_boot_active_panel`), and rebuilds no longer
                // touch it.
                //
                // An unknown panel version supplies no contract, so the rebuild
                // falls back to requiring that the requested version *is* the
                // active panel and fails closed (NO_ACTIVE_PANEL /
                // SEARCH_PANEL_MISMATCH) rather than silently rebuilding a
                // different panel — the same refusal as before.
                let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
                let supplied =
                    resolve_queryable_panel_contract(vault, expected_panel_version, created_at_ms)?;
                vault
                    .rebuild_search_indexes_for_panel(expected_panel_version, supplied.as_ref())
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_search",
                            "rebuild persisted Calyx search indexes",
                            &source,
                        )
                    })
            },
        )
    }

    fn propose_calyx_search_tuning(
        &self,
        expected_panel_version: u32,
        candidate: synapse_calyx::SynapseCalyxTuningConfig,
        description: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAnnealSearchReport> {
        self.with_vault(
            "calyx_anneal",
            "build and shadow-measure a Calyx search tuning candidate",
            true,
            |vault| {
                let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
                let panel = resolve_queryable_panel_contract(vault, expected_panel_version, created_at_ms)?
                    .ok_or_else(|| {
                        StorageError::BackendInvalidConfig {
                            value: expected_panel_version.to_string(),
                            detail: format!(
                                "SYNAPSE_CALYX_ANNEAL_PANEL_UNKNOWN: panel {expected_panel_version} has no declared contract; remediation=use a declared Synapse panel version"
                            ),
                        }
                    })?;
                vault
                    .anneal_propose_search_tuning(candidate.clone(), &panel, description)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_anneal",
                            "build and shadow-measure a Calyx search tuning candidate",
                            &source,
                        )
                    })
            },
        )
    }

    fn rollback_calyx_anneal(
        &self,
        change_id: u64,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAnnealRollbackReport> {
        self.with_vault(
            "calyx_anneal",
            "roll back a native Calyx Anneal change",
            true,
            |vault| {
                vault.anneal_rollback(change_id).map_err(|source| {
                    calyx_write_failed(
                        "calyx_anneal",
                        "roll back a native Calyx Anneal change",
                        &source,
                    )
                })
            },
        )
    }

    fn find_similar(
        &self,
        params: &SynapseCalyxFindParams,
    ) -> StorageResult<SynapseCalyxFindReport> {
        self.with_vault(
            "calyx_search",
            "run fused Calyx find-similar search",
            false,
            |vault| {
                // A find that names a non-active panel needs that panel's slot
                // map to measure its query through (#1668). synapse-calyx cannot
                // reconstruct it — `syn_queryable_panel_contract` lives here, in the
                // crate that depends on it — so the contract is resolved on this
                // side and handed down.
                //
                // `None` (no panel named) and an unknown version both hand down
                // no contract: the first queries the active panel exactly as
                // before, the second fails closed naming both lookups rather
                // than searching a panel whose slots were never validated.
                let supplied = match params.panel_version {
                    Some(version) => {
                        let created_at_ms = calyx_clock_now_for_read(vault, "calyx_manifest")?;
                        resolve_queryable_panel_contract(vault, version, created_at_ms)?
                    }
                    None => None,
                };
                vault
                    .find_similar_in_panel(params, supplied.as_ref())
                    .map_err(|source| {
                        calyx_read_failed(
                            "calyx_search",
                            "run fused Calyx find-similar search",
                            &source,
                        )
                    })
            },
        )
    }

    fn retire_search_generation(
        &self,
        panel_version: u32,
    ) -> StorageResult<(SynapseCalyxRetiredSearchGeneration, SupersededPanelLineage)> {
        retire_search_generation_on_vault(&self.vault, panel_version)
    }

    fn retire_orphan_slot_cfs(&self) -> StorageResult<AsterOrphanSlotGcReport> {
        retire_orphan_slot_cfs_on_vault(&self.vault)
    }

    fn close_calyx_vault(
        &self,
        reason: &'static str,
    ) -> StorageResult<SynapseCalyxVaultCloseReadback> {
        self.vault.close(reason, false)
    }

    fn close_calyx_vault_for_process_exit(
        &self,
        reason: &'static str,
    ) -> StorageResult<SynapseCalyxVaultCloseReadback> {
        self.vault.close(reason, true)
    }

    fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>> {
        self.with_vault(
            "<calyx-vault>",
            "inspect Calyx vault collections",
            false,
            |vault| inspect_calyx_vault(vault, &self.path).map(Some),
        )
    }

    fn backup_calyx_vault(
        &self,
        target_root: &Path,
        include_regenerable: bool,
    ) -> StorageResult<SynapseCalyxBackupReport> {
        self.with_vault("<calyx-vault>", "back up Calyx vault", true, |vault| {
            vault
                .backup(target_root, include_regenerable)
                .map_err(|source| {
                    calyx_write_failed("<calyx-vault>", "back up Calyx vault", &source)
                })
        })
    }

    fn verify_calyx_restore(&self, vault_path: &Path) -> StorageResult<SynapseCalyxVerifyReport> {
        synapse_calyx::verify_vault_restore(vault_path).map_err(|source| {
            calyx_read_failed("<calyx-vault>", "verify restored Calyx vault", &source)
        })
    }

    fn verify_calyx_vault(
        &self,
        full_chain: bool,
        tail_entries: u64,
    ) -> StorageResult<SynapseCalyxVaultVerifyReport> {
        self.with_vault("<calyx-vault>", "verify live Calyx vault", false, |vault| {
            vault
                .verify_vault(full_chain, tail_entries)
                .map_err(|source| {
                    calyx_read_failed("<calyx-vault>", "verify live Calyx vault", &source)
                })
        })
    }

    fn verify_calyx_ledger_chain(
        &self,
        range: Option<(u64, u64)>,
    ) -> StorageResult<SynapseCalyxLedgerVerifyReport> {
        self.with_vault(
            "calyx_ledger",
            "verify Calyx provenance ledger chain",
            false,
            |vault| {
                vault.verify_ledger_chain(range).map_err(|source| {
                    calyx_read_failed(
                        "calyx_ledger",
                        "verify Calyx provenance ledger chain",
                        &source,
                    )
                })
            },
        )
    }

    fn adjudicate_calyx_raw_commitment_seal(
        &self,
        ledger_seq: u64,
        expected_failure_sha256: &str,
        reason: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSealAdjudicationReceipt> {
        self.with_vault(
            "calyx_ledger",
            "adjudicate Calyx raw-commitment cohort seal",
            true,
            |vault| {
                vault
                    .adjudicate_raw_commitment_seal(ledger_seq, expected_failure_sha256, reason)
                    .map_err(|source| {
                        calyx_operation_failed(
                            "calyx_ledger",
                            true,
                            format!(
                                "adjudicate Calyx raw-commitment cohort seal {ledger_seq}: {}",
                                source.message
                            ),
                        )
                    })
            },
        )
    }

    fn read_calyx_ledger_entry(&self, seq: u64) -> StorageResult<SynapseCalyxLedgerEntryReadback> {
        self.with_vault(
            "calyx_ledger",
            "read Calyx provenance ledger entry",
            false,
            |vault| {
                vault.read_ledger_entry(seq).map_err(|source| {
                    calyx_read_failed(
                        "calyx_ledger",
                        "read Calyx provenance ledger entry",
                        &source,
                    )
                })
            },
        )
    }

    fn reproduce_calyx_record(&self, cx_id: &str) -> StorageResult<SynapseCalyxReproduceReport> {
        self.with_vault(
            "calyx_ledger",
            "reproduce Calyx record provenance",
            false,
            |vault| {
                vault.reproduce_record(cx_id).map_err(|source| {
                    calyx_read_failed("calyx_ledger", "reproduce Calyx record provenance", &source)
                })
            },
        )
    }

    fn read_calyx_base_source_pointer(
        &self,
        cx_id: &str,
    ) -> StorageResult<Option<synapse_calyx::SynapseCalyxBaseSourcePointer>> {
        let parsed =
            cx_id
                .trim()
                .parse::<calyx_core::CxId>()
                .map_err(|error| StorageError::ReadFailed {
                    cf_name: "base".to_owned(),
                    detail: format!("Calyx Base source-pointer id {cx_id:?} is invalid: {error}"),
                })?;
        self.with_vault("calyx_base", "read Base source pointer", false, |vault| {
            vault.read_base_source_pointer(parsed).map_err(|source| {
                calyx_write_failed("calyx_base", "read Base source pointer", &source)
            })
        })
    }

    fn erase_calyx_record(&self, cx_id: &str) -> StorageResult<SynapseCalyxErasureReport> {
        self.with_vault(
            "calyx_ledger",
            "erase Calyx record via ledger tombstone",
            true,
            |vault| {
                vault.erase_record(cx_id).map_err(|source| {
                    calyx_write_failed(
                        "calyx_ledger",
                        "erase Calyx record via ledger tombstone",
                        &source,
                    )
                })
            },
        )
    }

    fn put_recurrence_subject_occurrence(
        &self,
        kind: RecurrenceSubjectKind,
        subject_id: &str,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<CalyxRecurrenceSubjectReport> {
        self.with_vault(
            "calyx_recurrence",
            "put native Calyx recurrence subject occurrence",
            true,
            |vault| {
                put_recurrence_subject_occurrence_on_vault(
                    vault,
                    kind,
                    subject_id,
                    event_time_ns,
                    occurrence_identity,
                    context,
                    None,
                )
            },
        )
    }

    fn put_action_oracle_publication(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<ActionOraclePublicationReport> {
        self.vault.put_action_oracle_publication_inner(
            source_key,
            raw_bytes,
            record,
            event_time_ns,
            occurrence_identity,
            context,
        )
    }

    fn oracle_predict_action(&self, query_cx_id: &str) -> StorageResult<Value> {
        self.vault.oracle_predict_action(query_cx_id)
    }

    fn oracle_reverse_action(&self, outcome: bool) -> StorageResult<Value> {
        self.vault.oracle_reverse_action(outcome)
    }

    fn oracle_complete_action(&self, cx_id: &str, free_slots: &[u16]) -> StorageResult<Value> {
        self.vault.oracle_complete_action(cx_id, free_slots)
    }

    fn oracle_validate_action(&self) -> StorageResult<Value> {
        self.vault.oracle_validate_action()
    }

    fn oracle_measure_readiness(&self) -> StorageResult<Value> {
        self.vault.oracle_measure_readiness()
    }

    fn oracle_readiness(&self) -> StorageResult<Option<Value>> {
        self.vault.oracle_readiness()
    }

    fn append_autonomy_decision(
        &self,
        routine_id: &str,
        decision: &Value,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAutonomyDecisionReadback> {
        self.vault.append_autonomy_decision(routine_id, decision)
    }

    fn persist_recurrence_finding(
        &self,
        finding: &SynapseCalyxPersistedRecurrenceFinding,
    ) -> StorageResult<SynapseCalyxPersistedRecurrenceFinding> {
        self.with_vault(
            "calyx_reactive",
            "persist and read back routine recurrence finding",
            true,
            |vault| {
                vault.persist_recurrence_finding(finding).map_err(|source| {
                    calyx_write_failed(
                        "calyx_reactive",
                        "persist and read back routine recurrence finding",
                        &source,
                    )
                })
            },
        )
    }

    fn persist_novelty_finding(
        &self,
        finding: &SynapseCalyxPersistedNoveltyFinding,
    ) -> StorageResult<SynapseCalyxPersistedNoveltyFinding> {
        self.with_vault(
            "calyx_reactive",
            "persist and read back Ward novelty finding",
            true,
            |vault| {
                vault.persist_novelty_finding(finding).map_err(|source| {
                    calyx_write_failed(
                        "calyx_reactive",
                        "persist and read back Ward novelty finding",
                        &source,
                    )
                })
            },
        )
    }

    fn persisted_novelty_findings(
        &self,
        after_ledger_seq: u64,
        max_rows: usize,
    ) -> StorageResult<Vec<SynapseCalyxPersistedNoveltyFinding>> {
        self.with_vault(
            "calyx_reactive",
            "read persisted Ward novelty findings",
            false,
            |vault| {
                vault
                    .persisted_novelty_findings(after_ledger_seq, max_rows)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_reactive",
                            "read persisted Ward novelty findings",
                            &source,
                        )
                    })
            },
        )
    }

    fn novelty_delivery_cursor(&self) -> StorageResult<u64> {
        self.with_vault(
            "calyx_reactive",
            "read durable Ward novelty delivery cursor",
            false,
            |vault| {
                vault.novelty_delivery_cursor().map_err(|source| {
                    calyx_write_failed(
                        "calyx_reactive",
                        "read durable Ward novelty delivery cursor",
                        &source,
                    )
                })
            },
        )
    }

    fn persist_novelty_delivery_cursor(&self, ledger_seq: u64) -> StorageResult<u64> {
        self.with_vault(
            "calyx_reactive",
            "persist durable Ward novelty delivery cursor",
            false,
            |vault| {
                vault
                    .persist_novelty_delivery_cursor(ledger_seq)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_reactive",
                            "persist durable Ward novelty delivery cursor",
                            &source,
                        )
                    })
            },
        )
    }

    fn persisted_region_findings(
        &self,
        after_observed_seq: u64,
        max_rows: usize,
    ) -> StorageResult<Vec<SynapseCalyxPersistedRegionFinding>> {
        self.with_vault(
            "calyx_reactive",
            "read persisted first-observation region findings",
            false,
            |vault| {
                vault
                    .persisted_region_findings(after_observed_seq, max_rows)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_reactive",
                            "read persisted first-observation region findings",
                            &source,
                        )
                    })
            },
        )
    }

    fn region_delivery_cursor(&self) -> StorageResult<u64> {
        self.with_vault(
            "calyx_reactive",
            "read durable region delivery cursor",
            false,
            |vault| {
                vault.region_delivery_cursor().map_err(|source| {
                    calyx_write_failed(
                        "calyx_reactive",
                        "read durable region delivery cursor",
                        &source,
                    )
                })
            },
        )
    }

    fn persist_region_delivery_cursor(&self, observed_seq: u64) -> StorageResult<u64> {
        self.with_vault(
            "calyx_reactive",
            "persist durable region delivery cursor",
            false,
            |vault| {
                vault
                    .persist_region_delivery_cursor(observed_seq)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_reactive",
                            "persist durable region delivery cursor",
                            &source,
                        )
                    })
            },
        )
    }

    fn read_recurrence_subject_series(
        &self,
        kind: RecurrenceSubjectKind,
        subject_id: &str,
    ) -> StorageResult<SynapseCalyxRecurrenceSeriesReadback> {
        self.with_vault(
            "calyx_recurrence",
            "read native Calyx recurrence subject series",
            false,
            |vault| {
                let subject_id = validate_recurrence_subject_id(subject_id)?;
                let input = constellations::recurrence_subject_input_bytes(kind, &subject_id);
                let cx_id = vault.cx_id_for_input(&input, SYN_RECURRENCE_SUBJECT_PANEL_VERSION);
                vault.read_recurrence_series(cx_id).map_err(|source| {
                    calyx_write_failed(
                        "calyx_recurrence",
                        "read native Calyx recurrence subject series",
                        &source,
                    )
                })
            },
        )
    }

    fn list_temporal_panels(&self) -> StorageResult<Vec<VaultTemporalPanelRegistration>> {
        self.with_vault(
            "calyx_registry",
            "list native Calyx temporal panels",
            false,
            |vault| {
                vault.list_temporal_panels().map_err(|source| {
                    calyx_write_failed(
                        "calyx_registry",
                        "list native Calyx temporal panels",
                        &source,
                    )
                })
            },
        )
    }

    fn add_panel_lens(
        &self,
        panel_version: u32,
        operation_id: &str,
        slot_key: &str,
        lens_spec: calyx_registry::LensSpec,
        source_projection: synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection,
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxAddLensReadback> {
        const MAX_BACKFILL_CANDIDATES: usize = 100_000;
        self.with_vault(
            "calyx_registry",
            "add durable Calyx panel lens",
            true,
            |vault| {
                let entry = constellations::panel_catalog_entry_for_version(panel_version)
                    .filter(|entry| entry.panel_version == panel_version)
                    .ok_or_else(|| StorageError::BackendInvalidConfig {
                        value: panel_version.to_string(),
                        detail: "panel lifecycle mutations require an exact active built-in panel generation; superseded and unknown generations are immutable".to_owned(),
                    })?;
                let now = calyx_clock_now_for_write(vault, "calyx_registry")?;
                let contract = syn_reconstructable_panel_contract(panel_version, now)?.ok_or_else(|| {
                    StorageError::BackendInvalidConfig {
                        value: panel_version.to_string(),
                        detail: "the active panel has no reconstructable built-in contract; declare every slot runtime in syn_reconstructable_panel_contract before mutating it".to_owned(),
                    }
                })?;
                let candidates = vault
                    .panel_constellation_ids(panel_version, MAX_BACKFILL_CANDIDATES)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_base",
                            "enumerate bounded panel lifecycle backfill candidates",
                            &source,
                        )
                    })?;
                vault
                    .add_panel_lens(&synapse_calyx::panel_lifecycle::SynapseCalyxAddLensRequest {
                        panel_name: entry.panel_name,
                        base_panel: contract.panel,
                        operation_id,
                        slot_key,
                        lens_spec,
                        source_projection,
                        candidates: &candidates,
                        now,
                    })
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_registry",
                            "add durable Calyx panel lens",
                            &source,
                        )
                    })
            },
        )
    }

    fn set_panel_lens_state(
        &self,
        panel_version: u32,
        operation_id: &str,
        slot_id: calyx_core::SlotId,
        state: calyx_core::SlotState,
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxSetLensStateReadback> {
        self.with_vault(
            "calyx_registry",
            "change durable Calyx panel lens state",
            true,
            |vault| {
                let entry = constellations::panel_catalog_entry_for_version(panel_version)
                    .filter(|entry| entry.panel_version == panel_version)
                    .ok_or_else(|| StorageError::BackendInvalidConfig {
                        value: panel_version.to_string(),
                        detail: "panel lifecycle mutations require an exact active built-in panel generation; superseded and unknown generations are immutable".to_owned(),
                    })?;
                let now = calyx_clock_now_for_write(vault, "calyx_registry")?;
                vault
                    .set_panel_lens_state(
                        &synapse_calyx::panel_lifecycle::SynapseCalyxSetLensStateRequest {
                            panel_name: entry.panel_name,
                            operation_id,
                            slot_id,
                            state,
                            now,
                        },
                    )
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_registry",
                            "change durable Calyx panel lens state",
                            &source,
                        )
                    })
            },
        )
    }

    fn read_panel_lifecycle(
        &self,
        panel_version: u32,
    ) -> StorageResult<Option<synapse_calyx::panel_lifecycle::SynapseCalyxPanelLifecycleState>>
    {
        self.with_vault(
            "calyx_registry",
            "read durable Calyx panel lifecycle",
            false,
            |vault| {
                let entry = constellations::panel_catalog_entry_for_version(panel_version)
                    .filter(|entry| entry.panel_version == panel_version)
                    .ok_or_else(|| StorageError::BackendInvalidConfig {
                        value: panel_version.to_string(),
                        detail: "panel lifecycle reads require an exact active built-in panel generation; inspect the catalog for the current generation".to_owned(),
                    })?;
                vault.read_panel_lifecycle(entry.panel_name).map_err(|source| {
                    calyx_write_failed(
                        "calyx_registry",
                        "read durable Calyx panel lifecycle",
                        &source,
                    )
                })
            },
        )
    }

    fn publish_graph_position_snapshot(
        &self,
        kind: constellations::GraphPositionKind,
        source_seq: u64,
        created_at_ms: u64,
        transitions: &[(String, String, u64)],
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback> {
        self.with_vault(
            "calyx_graph",
            "publish atomic graph-position snapshot",
            true,
            |vault| {
                constellations::publish_graph_position_snapshot(
                    vault,
                    kind,
                    source_seq,
                    created_at_ms,
                    transitions,
                )
            },
        )
    }

    fn publish_path_hierarchy_snapshot(
        &self,
        source_seq: u64,
        created_at_ms: u64,
        paths: &[String],
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback> {
        self.with_vault(
            "calyx_graph",
            "publish atomic path-hierarchy snapshot",
            true,
            |vault| {
                constellations::publish_path_hierarchy_snapshot(
                    vault,
                    source_seq,
                    created_at_ms,
                    paths,
                )
            },
        )
    }

    fn supersede_panel_generations(
        &self,
        panel_name: &str,
        successor: u32,
    ) -> StorageResult<synapse_calyx::PanelGenerationSupersession> {
        self.with_vault(
            "calyx_registry",
            "retire the superseded generations of a dynamic panel",
            true,
            |vault| {
                vault
                    .supersede_panel_generations(panel_name, successor)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_registry",
                            "retire the superseded generations of a dynamic panel",
                            &source,
                        )
                    })
            },
        )
    }

    fn panel_generation_allocator(
        &self,
    ) -> StorageResult<synapse_calyx::PanelGenerationAllocatorReadback> {
        self.with_vault(
            "calyx_registry",
            "read the vault-global panel generation allocator",
            true,
            |vault| {
                vault.panel_generation_allocator().map_err(|source| {
                    calyx_write_failed(
                        "calyx_registry",
                        "read the vault-global panel generation allocator",
                        &source,
                    )
                })
            },
        )
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one confirmation loop with an explicit verdict per failure mode; collapsing the arms would replace named verdicts with a generic `unconfirmed`, which is the exact ambiguity #1899 exists to remove"
    )]
    fn confirm_exact_matches(
        &self,
        panel_version: u32,
        slot: u16,
        value: &str,
        cx_ids: &[String],
    ) -> StorageResult<Vec<constellations::ExactMatchConfirmation>> {
        use constellations::{
            EXACT_MATCH_BUCKET_COLLISION, EXACT_MATCH_CONFIRMED, ExactMatchConfirmation,
        };
        let lane = constellations::syn_exact_match_lane(panel_version, slot).ok_or_else(|| {
            StorageError::BackendInvalidConfig {
                value: format!("panel={panel_version} slot={slot}"),
                detail: "no exact-match lane is declared for this panel slot, so a hash-lane candidate cannot be confirmed against a source field; declare it in SYN_EXACT_MATCH_LANES beside its measurement site".to_owned(),
            }
        })?;
        let mut out = Vec::with_capacity(cx_ids.len());
        for cx_id in cx_ids {
            let parsed = cx_id.trim().parse::<calyx_core::CxId>().map_err(|error| {
                StorageError::ReadFailed {
                    cf_name: "base".to_owned(),
                    detail: format!("exact-match candidate {cx_id:?} is not a CxId: {error}"),
                }
            })?;
            let pointer = self.with_vault(
                "calyx_base",
                "read Base source pointer for exact-match confirmation",
                false,
                |vault| {
                    vault.read_base_source_pointer(parsed).map_err(|source| {
                        calyx_write_failed(
                            "calyx_base",
                            "read Base source pointer for exact-match confirmation",
                            &source,
                        )
                    })
                },
            )?;
            let Some(pointer) = pointer else {
                out.push(ExactMatchConfirmation {
                    cx_id: cx_id.clone(),
                    confirmed: false,
                    source_cf: None,
                    source_key_hex: None,
                    observed_value: None,
                    verdict: "base_row_absent",
                });
                continue;
            };
            if pointer.panel_version != panel_version {
                out.push(ExactMatchConfirmation {
                    cx_id: cx_id.clone(),
                    confirmed: false,
                    source_cf: pointer.source_cf,
                    source_key_hex: pointer.source_key_hex,
                    observed_value: None,
                    verdict: "panel_mismatch",
                });
                continue;
            }
            let (Some(source_cf), Some(source_key_hex)) =
                (pointer.source_cf.clone(), pointer.source_key_hex.clone())
            else {
                out.push(ExactMatchConfirmation {
                    cx_id: cx_id.clone(),
                    confirmed: false,
                    source_cf: pointer.source_cf,
                    source_key_hex: pointer.source_key_hex,
                    observed_value: None,
                    verdict: "source_row_absent",
                });
                continue;
            };
            if source_cf != lane.source_cf {
                out.push(ExactMatchConfirmation {
                    cx_id: cx_id.clone(),
                    confirmed: false,
                    source_cf: Some(source_cf),
                    source_key_hex: Some(source_key_hex),
                    observed_value: None,
                    verdict: "source_cf_mismatch",
                });
                continue;
            }
            let key = decode_source_key_hex(&source_key_hex).map_err(|detail| {
                StorageError::ReadFailed {
                    cf_name: source_cf.clone(),
                    detail: format!(
                        "exact-match candidate {cx_id} records an undecodable source key: {detail}"
                    ),
                }
            })?;
            let Some(raw) = self.get_cf(&source_cf, &key)? else {
                out.push(ExactMatchConfirmation {
                    cx_id: cx_id.clone(),
                    confirmed: false,
                    source_cf: Some(source_cf),
                    source_key_hex: Some(source_key_hex),
                    observed_value: None,
                    verdict: "source_row_absent",
                });
                continue;
            };
            let observed =
                constellations::syn_exact_field_value(&source_cf, lane.field_path, &raw)?;
            let verdict = match observed.as_deref() {
                Some(found) if found == value => EXACT_MATCH_CONFIRMED,
                Some(_) => EXACT_MATCH_BUCKET_COLLISION,
                None => "source_field_absent",
            };
            out.push(ExactMatchConfirmation {
                cx_id: cx_id.clone(),
                confirmed: verdict == EXACT_MATCH_CONFIRMED,
                source_cf: Some(source_cf),
                source_key_hex: Some(source_key_hex),
                observed_value: observed,
                verdict,
            });
        }
        Ok(out)
    }

    fn temporal_rerank(
        &self,
        candidates: &[SynapseCalyxTemporalCandidate],
        query_time_secs: i64,
        tz_offset_secs: i32,
    ) -> StorageResult<SynapseCalyxTemporalRerankReadback> {
        self.with_vault(
            "calyx_registry",
            "apply registered Calyx temporal rerank",
            false,
            |vault| {
                vault
                    .temporal_rerank_registered(candidates, query_time_secs, tz_offset_secs)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_registry",
                            "apply registered Calyx temporal rerank",
                            &source,
                        )
                    })
            },
        )
    }

    fn weave_panel_intelligence(
        &self,
        params: SynapseCalyxWeaveParams,
    ) -> StorageResult<SynapseCalyxWeaveReport> {
        self.with_vault(
            "calyx_loom",
            "weave native Calyx panel associations",
            true,
            |vault| {
                vault.weave_panel(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_loom",
                        "weave native Calyx panel associations",
                        &source,
                    )
                })
            },
        )
    }

    fn publish_panel_input_snapshot(
        &self,
        panel_version: u32,
        chunk_rows: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPanelInputSnapshotReport> {
        self.with_vault(
            "calyx_loom",
            "publish durable panel association-input snapshot",
            true,
            |vault| {
                vault
                    .publish_panel_input_snapshot(panel_version, chunk_rows)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_loom",
                            "publish durable panel association-input snapshot",
                            &source,
                        )
                    })
            },
        )
    }

    fn prune_panel_input_changes(
        &self,
        panel_version: u32,
        through_seq: u64,
        mutation_through_seq: u64,
        max_rows: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPanelInputPruneReport> {
        self.with_vault(
            "calyx_loom",
            "prune acknowledged panel association-input changes",
            true,
            |vault| {
                vault
                    .prune_panel_input_changes(
                        panel_version,
                        through_seq,
                        mutation_through_seq,
                        max_rows,
                    )
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_loom",
                            "prune acknowledged panel association-input changes",
                            &source,
                        )
                    })
            },
        )
    }

    fn commission_search_kernels(
        &self,
        params: &SynapseCalyxSearchCommissionParams,
    ) -> StorageResult<SynapseCalyxSearchCommissionReport> {
        self.with_vault(
            "calyx_search_commission",
            "commission optimized Calyx search kernels",
            true,
            |vault| {
                vault.commission_search_kernels(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_search_commission",
                        "commission optimized Calyx search kernels",
                        &source,
                    )
                })
            },
        )
    }

    fn abundance_report_intelligence(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> StorageResult<SynapseCalyxAbundanceReport> {
        self.with_vault(
            "calyx_loom",
            "read native Calyx abundance report",
            false,
            |vault| {
                vault
                    .abundance_report(panel_version, max_records)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_loom",
                            "read native Calyx abundance report",
                            &source,
                        )
                    })
            },
        )
    }

    fn assay_bits_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseCalyxBitsReport> {
        refuse_outcome_query_on_observation_panel("bits", params)?;
        self.with_vault(
            "calyx_assay",
            "measure native Calyx lens bits",
            true,
            |vault| {
                vault.assay_bits(params).map_err(|source| {
                    calyx_write_failed("calyx_assay", "measure native Calyx lens bits", &source)
                })
            },
        )
    }

    fn assay_sufficiency_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseCalyxSufficiencyReport> {
        refuse_outcome_query_on_observation_panel("sufficiency", params)?;
        self.with_vault(
            "calyx_assay",
            "measure native Calyx panel sufficiency",
            true,
            |vault| {
                vault.assay_sufficiency(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx panel sufficiency",
                        &source,
                    )
                })
            },
        )
    }

    fn assay_ensemble_card_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> StorageResult<SynapseCalyxEnsembleCardReport> {
        self.with_vault(
            "calyx_assay",
            "measure the native Calyx ensemble capability card",
            true,
            |vault| {
                vault
                    .assay_ensemble_card(params, min_gate_lenses)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_assay",
                            "measure the native Calyx ensemble capability card",
                            &source,
                        )
                    })
            },
        )
    }

    fn measure_causal_view_registry_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> StorageResult<SynapseCalyxCausalViewRegistryReadback> {
        self.with_vault(
            "calyx_registry",
            "measure and atomically publish the native Calyx causal-view registry",
            true,
            |vault| {
                let withheld_predictor_slots = ACTION_CAUSAL_PREDICTOR_SLOTS
                    .iter()
                    .copied()
                    .filter(|slot| params.excluded_slots.contains(slot))
                    .collect::<Vec<_>>();
                if !withheld_predictor_slots.is_empty() {
                    let source = SynapseCalyxError::new(
                        "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PREDICTOR_VIEW_EXCLUDED",
                        format!(
                            "canonical action Registry request withheld predictor slots {withheld_predictor_slots:?}"
                        ),
                        "remove every frozen action predictor slot from excluded_slots; only declared anchor carriers and parked collection-only views may be withheld",
                    );
                    return Err(calyx_write_failed(
                        "calyx_registry",
                        "validate canonical Registry predictor roster",
                        &source,
                    ));
                }
                let mut bound_params = params.clone();
                bound_params.physical_lens_bindings =
                    crate::constellations::syn_action_causal_physical_lens_bindings()?;
                bound_params.required_record_slots = std::iter::once(
                    synapse_calyx::SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT,
                )
                .collect();
                vault
                    .measure_causal_view_registry(&bound_params, min_gate_lenses)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_registry",
                            "measure and atomically publish the native Calyx causal-view registry",
                            &source,
                        )
                    })
            },
        )
    }

    fn read_causal_view_registry_intelligence(
        &self,
        scope: &SynapseCalyxCausalViewRegistryScope,
    ) -> StorageResult<Option<SynapseCalyxCausalViewRegistryReadback>> {
        self.with_vault(
            "calyx_registry",
            "read the persisted native Calyx causal-view registry",
            false,
            |vault| {
                if scope.panel_version == SYN_ACTION_PANEL_VERSION
                    && scope.corpus_shard == "synapse.action"
                    && scope.anchor_kind == "reward"
                {
                    return current_action_causal_registry(vault).map(Some);
                }
                vault.read_causal_view_registry(scope).map_err(|source| {
                    calyx_read_failed(
                        "calyx_registry",
                        "read the persisted native Calyx causal-view registry",
                        &source,
                    )
                })
            },
        )
    }

    fn assay_redundancy_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseCalyxRedundancyReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx lens redundancy",
            true,
            |vault| {
                vault.assay_redundancy(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx lens redundancy",
                        &source,
                    )
                })
            },
        )
    }

    fn assay_synergy_intelligence(
        &self,
        params: &SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseSynergyReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx lens synergy",
            true,
            |vault| {
                let annotated = vault.assay_synergy(params).map_err(|source| {
                    calyx_write_failed("calyx_assay", "measure native Calyx lens synergy", &source)
                })?;
                // #1670: every assay result over an under-anchored domain is
                // tagged provisional, so the synergy pass carries the same
                // verdict the bits/sufficiency/redundancy reports do.
                let verdict = vault
                    .domain_grounding_verdict(params.panel_version, params.max_records)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_assay",
                            "read native Calyx domain grounding verdict",
                            &source,
                        )
                    })?;
                // Physical readback: the Assay CF row count after the pass.
                let assay_cf_rows_after = vault
                    .scan_cf_latest(ColumnFamily::Assay)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_assay",
                            "read back native Calyx Assay CF",
                            &source,
                        )
                    })?
                    .len();
                let report = &annotated.report;
                Ok(SynapseSynergyReport {
                    panel_version: report.panel_version,
                    anchor_kind: params.anchor_kind.clone(),
                    anchored_records: report.anchored_records,
                    n_lenses: report.n_lenses,
                    lenses_paired: report.lenses_paired,
                    pairs_evaluated: report.pairs_evaluated,
                    pairs_unmeasured: report.pairs_unmeasured,
                    pairs_cross_estimator_unpinnable: report.pairs_cross_estimator_unpinnable,
                    pairs_monotonicity_floored: report.pairs_monotonicity_floored,
                    synergistic_pairs: report.synergistic_pairs,
                    max_gain_bits: report.max_gain_bits,
                    max_gain_bits_carrier_free: annotated.max_gain_bits_carrier_free,
                    anchor_source_declared: annotated.anchor_source_declared,
                    anchor_source_carriers: annotated
                        .anchor_source_carriers
                        .iter()
                        .cloned()
                        .map(SynapseAnchorSourceCarrier::from)
                        .collect(),
                    pairs_with_anchor_source_carrier: annotated.pairs_with_anchor_source_carrier,
                    domain_provisional: verdict.provisional,
                    domain_grounded_fraction: verdict.grounded_fraction,
                    pairs: report
                        .pairs
                        .iter()
                        .cloned()
                        .map(|pair| SynapseSynergyPair {
                            slot_a: pair.a.get(),
                            slot_b: pair.b.get(),
                            anchor_source_carrier_slots: annotated
                                .carrier_slots_in_pair(pair.a.get(), pair.b.get()),
                            pair_bits: pair.pair_bits,
                            left_bits: pair.left_bits,
                            right_bits: pair.right_bits,
                            gain_bits: pair.gain_bits,
                            raw_gain_bits: pair.raw_gain_bits,
                            monotonicity_floor_applied: pair.monotonicity_floor_applied,
                            pair_estimator: pair.estimators.map(|e| e.pair.as_str().to_owned()),
                            left_estimator: pair.estimators.map(|e| e.left.as_str().to_owned()),
                            right_estimator: pair.estimators.map(|e| e.right.as_str().to_owned()),
                            n_samples: pair.n_samples,
                            synergistic: pair.synergistic,
                            provisional: pair.provisional,
                            state: pair.state.as_str().to_owned(),
                            unmeasured_reason: pair.unmeasured_reason,
                        })
                        .collect(),
                    assay_cf_rows_after,
                })
            },
        )
    }

    fn temporal_causality_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxCausalityReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx temporal causality",
            true,
            |vault| {
                vault
                    .ensure_event_time_index(params.panel_version)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_index_btree",
                            "ensure exact event-time index before temporal causality",
                            &source,
                        )
                    })?;
                vault.temporal_causality(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx temporal causality",
                        &source,
                    )
                })
            },
        )
    }

    fn temporal_causal_map_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
        fdr_alpha: f32,
    ) -> StorageResult<SynapseCalyxCausalMapReport> {
        self.with_vault(
            "calyx_assay",
            "measure exhaustive native Calyx causal map",
            true,
            |vault| {
                // Finite-only temporal panels deliberately have no ANN/search
                // generation. They still require an exact panel-membership
                // sidecar for bounded exhaustive analytics. Prepare that
                // first-class membership generation here, on the mutating
                // producer path; the independent causal_map_read path below
                // remains strictly read-only and fails closed if it is absent
                // or corrupt.
                if !crate::constellations::syn_panel_is_queryable(params.panel_version) {
                    vault
                        .ensure_panel_membership_generation(params.panel_version)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_search",
                                "ensure finite-only panel membership for exhaustive causal map",
                                &source,
                            )
                        })?;
                }
                vault
                    .ensure_event_time_index(params.panel_version)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_index_btree",
                            "ensure exact event-time index before exhaustive causal map",
                            &source,
                        )
                    })?;
                vault
                    .temporal_causal_map(params, fdr_alpha)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_assay",
                            "measure exhaustive native Calyx causal map",
                            &source,
                        )
                    })
            },
        )
    }

    fn read_temporal_causal_map_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
        fdr_alpha: f32,
    ) -> StorageResult<SynapseCalyxCausalMapReport> {
        self.with_vault(
            "calyx_graph",
            "read persisted exhaustive native Calyx causal map",
            false,
            |vault| {
                vault
                    .read_temporal_causal_map(params, fdr_alpha)
                    .map_err(|source| {
                        calyx_read_failed(
                            "calyx_graph",
                            "read persisted exhaustive native Calyx causal map",
                            &source,
                        )
                    })
            },
        )
    }

    fn temporal_periodicity_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxPeriodicityReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx temporal periodicity",
            true,
            |vault| {
                vault
                    .ensure_event_time_index(params.panel_version)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_index_btree",
                            "ensure exact event-time index before periodicity",
                            &source,
                        )
                    })?;
                vault.temporal_periodicity(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx temporal periodicity",
                        &source,
                    )
                })
            },
        )
    }

    fn temporal_drift_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxDriftReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx temporal drift",
            true,
            |vault| {
                vault
                    .ensure_event_time_index(params.panel_version)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_index_btree",
                            "ensure exact event-time index before drift",
                            &source,
                        )
                    })?;
                vault.temporal_drift(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx temporal drift",
                        &source,
                    )
                })
            },
        )
    }

    fn temporal_hazard_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxHazardReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx temporal overdue hazard",
            true,
            |vault| {
                vault
                    .ensure_event_time_index(params.panel_version)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_index_btree",
                            "ensure exact event-time index before overdue hazard",
                            &source,
                        )
                    })?;
                vault.temporal_hazard(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx temporal overdue hazard",
                        &source,
                    )
                })
            },
        )
    }

    fn build_domain_kernel_intelligence(
        &self,
        params: &SynapseCalyxKernelParams,
    ) -> StorageResult<SynapseCalyxKernelReport> {
        self.with_vault(
            "calyx_lodestar",
            "build native Calyx grounding kernel",
            true,
            |vault| {
                vault.build_domain_kernel(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_lodestar",
                        "build native Calyx grounding kernel",
                        &source,
                    )
                })
            },
        )
    }

    fn kernel_answer_intelligence(
        &self,
        params: &SynapseCalyxKernelParams,
        query_cx_id: &str,
        max_hops: usize,
    ) -> StorageResult<SynapseCalyxKernelAnswerReport> {
        self.with_vault(
            "calyx_lodestar",
            "answer grounded query through native Calyx kernel",
            false,
            |vault| {
                vault
                    .kernel_answer(params, query_cx_id, max_hops)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_lodestar",
                            "answer grounded query through native Calyx kernel",
                            &source,
                        )
                    })
            },
        )
    }

    fn grounding_gap_intelligence(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> StorageResult<SynapseCalyxGroundingGapReport> {
        self.with_vault(
            "calyx_lodestar",
            "report native Calyx grounding gaps",
            false,
            |vault| {
                vault
                    .grounding_gap_report(panel_version, max_records)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_lodestar",
                            "report native Calyx grounding gaps",
                            &source,
                        )
                    })
            },
        )
    }

    fn blind_spot_intelligence(
        &self,
        params: &SynapseCalyxBlindSpotParams,
    ) -> StorageResult<SynapseCalyxBlindSpotReport> {
        self.with_vault(
            "calyx_loom",
            "scan native Calyx blind spots",
            false,
            |vault| {
                vault.blind_spot_scan(params).map_err(|source| {
                    calyx_write_failed("calyx_loom", "scan native Calyx blind spots", &source)
                })
            },
        )
    }

    fn panel_drift_intelligence(
        &self,
        params: &SynapseCalyxPanelDriftParams,
    ) -> StorageResult<SynapseCalyxPanelDriftReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx panel MMD drift",
            true,
            |vault| {
                vault.mmd_panel_drift(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_assay",
                        "measure native Calyx panel MMD drift",
                        &source,
                    )
                })
            },
        )
    }

    fn rebuild_domain_kernels_intelligence(
        &self,
        params: &SynapseCalyxKernelRebuildParams,
    ) -> StorageResult<SynapseCalyxKernelRebuildReport> {
        self.with_vault(
            "calyx_lodestar",
            "rebuild native Calyx per-domain grounding kernels",
            true,
            |vault| {
                vault.rebuild_domain_kernels(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_lodestar",
                        "rebuild native Calyx per-domain grounding kernels",
                        &source,
                    )
                })
            },
        )
    }

    fn domain_kernel_health_intelligence(
        &self,
        panel_version: u32,
        content_slot: u16,
        anchor_kind: Option<&str>,
    ) -> StorageResult<SynapseCalyxKernelHealthReport> {
        self.with_vault(
            "calyx_lodestar",
            "report native Calyx kernel health",
            false,
            |vault| {
                vault
                    .domain_kernel_health(panel_version, content_slot, anchor_kind)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_lodestar",
                            "report native Calyx kernel health",
                            &source,
                        )
                    })
            },
        )
    }

    fn guard_calibrate_intelligence(
        &self,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> StorageResult<SynapseCalyxGuardCalibrateReport> {
        // #1919: Synapse owns the catalogue of code-declared panels; the vault
        // only knows the single `Panel` its manifest publishes. Reconstruct the
        // requested generation's definition here and hand it down, so the guard
        // can calibrate against a panel that carries adjudicated outcomes
        // without that panel having to become the active one. An unknown
        // generation supplies nothing and the vault's own active-panel rule
        // still applies unchanged.
        self.with_vault(
            "calyx_ward",
            "calibrate the native Calyx Ward guard profile",
            true,
            |vault| {
                let mut params = params.clone();
                if params.calibration_panel.is_none() {
                    let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
                    params.calibration_panel =
                        syn_queryable_panel_contract(params.panel_version, created_at_ms)?
                            .map(|contract| contract.panel);
                }
                vault.guard_calibrate(&params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_ward",
                        "calibrate the native Calyx Ward guard profile",
                        &source,
                    )
                })
            },
        )
    }

    fn guard_verify_intelligence(
        &self,
        params: &SynapseCalyxGuardVerifyParams,
    ) -> StorageResult<SynapseCalyxGuardVerifyReport> {
        self.with_vault(
            "calyx_ward",
            "verify a record against the native Calyx Ward guard profile",
            false,
            |vault| {
                vault.guard_verify(params).map_err(|source| {
                    calyx_write_failed(
                        "calyx_ward",
                        "verify a record against the native Calyx Ward guard profile",
                        &source,
                    )
                })
            },
        )
    }

    #[allow(clippy::too_many_lines)]
    fn backfill_temporal_metadata(
        &self,
        source_cf: &str,
        source_keys: Option<&[Vec<u8>]>,
        pointwise_preflight: bool,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<constellations::TemporalMetadataBackfillReport> {
        // Every panel whose constellation can be rebuilt from one authoritative
        // source row. `builtin_panel_catalog`'s `backfill_source_cf` must name
        // exactly this set: a panel that declares a path this match rejects
        // would report a backfill as available and then fail every attempt
        // (#1965).
        if !matches!(
            source_cf,
            cf::CF_TIMELINE
                | cf::CF_EPISODES
                | cf::CF_AGENT_TRANSCRIPTS
                | cf::CF_AGENT_EVENTS
                | cf::CF_ACTION_LOG
                | cf::CF_REFLEX_AUDIT
                | cf::CF_PROCESS_HISTORY
                | cf::CF_OBSERVATIONS
                | SYN_MCP_USAGE_BACKFILL_SOURCE
                | SYN_OUTCOME_BACKFILL_SOURCE
        ) {
            return Err(StorageError::BackendInvalidConfig {
                value: source_cf.to_owned(),
                detail: "temporal metadata backfill source is not a declared rebuildable panel population"
                    .to_owned(),
            });
        }
        if source_keys.is_some() && after_physical.is_some() {
            return Err(StorageError::BackendInvalidConfig {
                value: source_cf.to_owned(),
                detail: "exact source_key and physical page cursor are mutually exclusive"
                    .to_owned(),
            });
        }
        let mut row_failures = Vec::new();
        let (rows, resume_after_physical, more, candidate_rows_examined, expired_rows_skipped) =
            if let Some(keys) = source_keys {
                if keys.is_empty() {
                    return Err(StorageError::BackendInvalidConfig {
                        value: source_cf.to_owned(),
                        detail: "exact temporal metadata backfill batch must contain at least one source key"
                            .to_owned(),
                    });
                }
                let mut seen = BTreeSet::new();
                for key in keys {
                    if !seen.insert(key.as_slice()) {
                        return Err(StorageError::BackendInvalidConfig {
                            value: constellations::hex_encode(key),
                            detail: "exact temporal metadata backfill batch contains a duplicate source key"
                                .to_owned(),
                        });
                    }
                }
                let mut exact_rows = Vec::with_capacity(keys.len());
                let physical_source_cf = backfill_physical_source_cf(source_cf);
                for key in keys {
                    if source_cf == SYN_MCP_USAGE_BACKFILL_SOURCE
                        && !key.starts_with(SYN_MCP_USAGE_KEY_PREFIX)
                    {
                        let error = StorageError::BackendInvalidConfig {
                        value: constellations::hex_encode(key),
                        detail: "exact MCP-usage backfill key is outside mcp-usage/v1/; remediation=pass a key from the declared prefix"
                            .to_owned(),
                    };
                        if pointwise_preflight {
                            row_failures.push(constellations::TemporalMetadataBackfillRowFailure {
                                source_key: key.clone(),
                                error: error.to_string(),
                            });
                            continue;
                        }
                        return Err(error);
                    }
                    // #1984: any DECLARED outcome family, not one of them. The
                    // single-prefix test here refused the panel's own repair queue —
                    // the census named a stranded identity under
                    // `approval/v1/audit/`, which is as much this panel's population
                    // as `escalation/v1/audit/` is, and the repair could not use the
                    // path the catalog gave it. See `SYN_OUTCOME_KEY_PREFIXES` for
                    // the writer-by-writer evidence that the declaration, not the
                    // identity, was the thing that was wrong.
                    if source_cf == SYN_OUTCOME_BACKFILL_SOURCE
                        && constellations::outcome_backfill_prefix_for_key(key).is_none()
                    {
                        let error = StorageError::BackendInvalidConfig {
                            value: constellations::hex_encode(key),
                            detail: format!(
                                "exact outcome backfill key is outside every declared outcome row family ({}); remediation=pass a key from one of the declared prefixes, or add this row family to SYN_OUTCOME_KEY_PREFIXES if a writer really measures it into syn-outcome-v1",
                                constellations::outcome_backfill_prefixes_display()
                            ),
                        };
                        if pointwise_preflight {
                            row_failures.push(constellations::TemporalMetadataBackfillRowFailure {
                                source_key: key.clone(),
                                error: error.to_string(),
                            });
                            continue;
                        }
                        return Err(error);
                    }
                    match self.get_cf(physical_source_cf, key) {
                        Ok(Some(value)) => exact_rows.push((key.clone(), value)),
                        Ok(None) => {
                            let error = StorageError::ReadFailed {
                                cf_name: source_cf.to_owned(),
                                detail: format!(
                                    "temporal metadata backfill source row not found: key_hex={}",
                                    constellations::hex_encode(key)
                                ),
                            };
                            if pointwise_preflight {
                                row_failures.push(
                                    constellations::TemporalMetadataBackfillRowFailure {
                                        source_key: key.clone(),
                                        error: error.to_string(),
                                    },
                                );
                            } else {
                                return Err(error);
                            }
                        }
                        Err(error) if pointwise_preflight => {
                            row_failures.push(constellations::TemporalMetadataBackfillRowFailure {
                                source_key: key.clone(),
                                error: error.to_string(),
                            });
                        }
                        Err(error) => return Err(error),
                    }
                }
                (exact_rows, None, false, keys.len(), 0)
            } else {
                let page = match source_cf {
                    // One contiguous declared prefix, so the physical range can
                    // be sought directly and no unrelated row is ever read.
                    SYN_MCP_USAGE_BACKFILL_SOURCE => self.with_vault(
                        cf::CF_KV,
                        "scan candidate-bounded MCP-usage prefix page",
                        false,
                        |vault| {
                            read_physical_prefix_page_from_vault(
                                vault,
                                cf::CF_KV,
                                SYN_MCP_USAGE_KEY_PREFIX,
                                after_physical,
                                max_rows,
                            )
                        },
                    )?,
                    // #1984: FOUR disjoint declared families, so a single
                    // seekable range does not describe the population. The page
                    // is taken over the physical `CF_KV` order — which is what
                    // the opaque resume cursor already means — and filtered to
                    // the declared families, so the sweep covers every row this
                    // panel actually owns instead of the one family that
                    // happened to be named. `candidate_rows_examined` is
                    // deliberately left as the UNFILTERED count: it is the
                    // measure of what the sweep had to look at, and hiding it
                    // would make a cheap sweep and an expensive one read alike.
                    SYN_OUTCOME_BACKFILL_SOURCE => {
                        let mut page =
                            self.scan_cf_physical_page(cf::CF_KV, after_physical, max_rows)?;
                        page.rows.retain(|(key, _)| {
                            constellations::outcome_backfill_prefix_for_key(key).is_some()
                        });
                        page
                    }
                    _ => self.scan_cf_physical_page(source_cf, after_physical, max_rows)?,
                };
                (
                    page.rows,
                    page.resume_after_physical,
                    page.more,
                    page.candidate_rows_examined,
                    page.expired_rows_skipped,
                )
            };
        let rows = if source_cf == cf::CF_OBSERVATIONS {
            let mut sampled = Vec::with_capacity(rows.len());
            for row in rows {
                if constellations::observation_constellation_sample_permits(&row.0)? {
                    sampled.push(row);
                } else if source_keys.is_some() {
                    let error = StorageError::BackendInvalidConfig {
                        value: constellations::hex_encode(&row.0),
                        detail: "exact CF_OBSERVATIONS row is outside the deterministic sampled panel population"
                            .to_owned(),
                    };
                    if pointwise_preflight {
                        row_failures.push(constellations::TemporalMetadataBackfillRowFailure {
                            source_key: row.0,
                            error: error.to_string(),
                        });
                    } else {
                        return Err(error);
                    }
                }
            }
            sampled
        } else {
            rows
        };
        if rows.is_empty() {
            let latest_seq = self.with_vault(
                "calyx_temporal_metadata_backfill",
                "read empty Calyx temporal metadata backfill sequence",
                false,
                |vault| Ok(vault.latest_seq()),
            )?;
            let report = constellations::TemporalMetadataBackfillReport {
                source_cf: source_cf.to_owned(),
                examined_rows: 0,
                inserted_rows: 0,
                backfilled_rows: 0,
                already_current_rows: 0,
                temporal_ineligible_rows: 0,
                outcome_anchored_rows: 0,
                outcome_absent_rows: 0,
                outcome_unadjudicable_rows: 0,
                anchors_carried_forward: 0,
                rows_anchor_carried: 0,
                anchor_carry_source_generations_read: 0,
                candidate_rows_examined: candidate_rows_examined as u64,
                expired_rows_skipped: expired_rows_skipped as u64,
                latest_seq,
                resume_after_physical,
                more,
                row_reports: Vec::new(),
                row_failures,
            };
            if (!pointwise_preflight && source_keys.is_some()) || (source_keys.is_none() && !more) {
                self.release_anchor_carry_lineage(Some(source_cf));
            }
            return Ok(report);
        }
        let examined_rows = rows.len() as u64;
        let mut inserted_rows = 0_u64;
        let mut backfilled_rows = 0_u64;
        let mut already_current_rows = 0_u64;
        let mut temporal_ineligible_rows = 0_u64;
        let mut outcome_anchored_rows = 0_u64;
        let mut outcome_absent_rows = 0_u64;
        let mut outcome_unadjudicable_rows = 0_u64;
        // #1980. Reported separately from `outcome_anchored_rows`, which counts
        // outcomes DERIVED from the source row on this pass. These are outcomes
        // the corpus already observed and would otherwise have lost, and
        // conflating the two would hide a carry that silently stopped working.
        let mut anchors_carried_forward = 0_u64;
        let mut rows_anchor_carried = 0_u64;
        let mut anchor_carry_source_generations_read = 0_u64;
        // #1981/#1982: build once at the head of a paged sweep, then retain the
        // exact historical Base-row lineage across later pages. Recomputing an
        // old cx_id from a mutable row's current bytes names a record that never
        // existed; probing it 100k times was both slow and always empty.
        let active_panel_version = backfill_panel_version(source_cf)?;
        let carry_superseded_anchors = constellations::builtin_panel_catalog()
            .into_iter()
            .find(|entry| entry.panel_version == active_panel_version)
            .ok_or_else(|| StorageError::BackendInvalidConfig {
                value: active_panel_version.to_string(),
                detail: format!(
                    "active backfill panel for source {source_cf} is absent from builtin_panel_catalog"
                ),
            })?
            .carry_superseded_anchors;
        let anchor_lineage = if carry_superseded_anchors {
            // Reset the lineage cache only at the head of a PAGED sweep. An
            // exact-key repair call (#1984's identity-driven anchor-debt queue)
            // always carries `after_physical=None` because the two are mutually
            // exclusive, and treating that as "new sweep" rebuilt the full-Base
            // lineage index once per identity — turning a debt-proportional
            // repair back into a corpus-proportional one
            // (STORAGE_DERIVED_STATE_ANCHOR_DEBT_REPAIR_UNAMORTIZED). Exact-key
            // calls reuse the cache; the cache's cf/superseded-version match
            // still forces a rebuild whenever the panel contract changed.
            self.anchor_carry_lineage(source_cf, source_keys.is_none() && after_physical.is_none())?
        } else {
            Arc::new(BTreeMap::new())
        };
        let superseded_generation_count = if carry_superseded_anchors {
            constellations::superseded_panel_versions_for_source_cf(source_cf)?.len() as u64
        } else {
            0
        };
        // #2105/#2121: lens construction is independent by source row, while
        // every durable write must remain ordered and serialized. Rayon indexed
        // collection preserves physical page order. We collect `Vec<Result<_>>`
        // first and resolve it sequentially so simultaneous failures still
        // report the first physical row rather than Rayon's nondeterministic
        // first completed error.
        let (page_outcomes, committed_row_indexes, build_failures) = self.with_vault(
            "calyx_temporal_metadata_backfill",
            "parallel-build and batch-commit Calyx temporal metadata page",
            true,
            |vault| {
                let build_context = TemporalBackfillBuildContext {
                    source_cf,
                    active_panel_version,
                    vault_id: vault.vault_id_value(),
                    created_at_ms: calyx_clock_now_for_write(
                        vault,
                        backfill_physical_source_cf(source_cf),
                    )?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let built = rows
                    .par_iter()
                    .map(|(key, raw)| {
                        build_temporal_backfill_row(vault, build_context, key, raw)
                    })
                    .collect::<Vec<_>>();
                let (mut prepared, mut committed_row_indexes, mut build_failures) = if pointwise_preflight {
                    let mut prepared = Vec::with_capacity(built.len());
                    let mut committed_row_indexes = Vec::with_capacity(built.len());
                    let mut build_failures = Vec::new();
                    for (row_index, result) in built.into_iter().enumerate() {
                        match result {
                            Ok(row) => {
                                prepared.push(row);
                                committed_row_indexes.push(row_index);
                            }
                            Err(error) => build_failures.push(
                                constellations::TemporalMetadataBackfillRowFailure {
                                    source_key: rows[row_index].0.clone(),
                                    error: error.to_string(),
                                },
                            ),
                        }
                    }
                    (prepared, committed_row_indexes, build_failures)
                } else {
                    let prepared = built
                        .into_iter()
                        .collect::<StorageResult<Vec<PreparedTemporalBackfillRow>>>()?;
                    let committed_row_indexes = (0..prepared.len()).collect::<Vec<_>>();
                    (prepared, committed_row_indexes, Vec::new())
                };
                if pointwise_preflight {
                    let mut eligible_prepared = Vec::with_capacity(prepared.len());
                    let mut eligible_indexes = Vec::with_capacity(committed_row_indexes.len());
                    for (row, source_row_index) in
                        prepared.into_iter().zip(committed_row_indexes)
                    {
                        let (_identity, temporal) =
                            constellations::temporal_migration_metadata(&row.constellation);
                        match constellations::temporal_migration_eligible(&temporal) {
                            Ok(_eligible) => {
                                eligible_prepared.push(row);
                                eligible_indexes.push(source_row_index);
                            }
                            Err(error) => build_failures.push(
                                constellations::TemporalMetadataBackfillRowFailure {
                                    source_key: rows[source_row_index].0.clone(),
                                    error: error.to_string(),
                                },
                            ),
                        }
                    }
                    prepared = eligible_prepared;
                    committed_row_indexes = eligible_indexes;
                }
                if prepared.is_empty() {
                    return Ok((Vec::new(), committed_row_indexes, build_failures));
                }
                let plans = prepared
                    .iter()
                    .map(|row| &row.measurement_plan)
                    .collect::<Vec<_>>();
                let replacements = constellations::resolve_deferred_measurement_plans(
                    active_panel_version,
                    build_context.created_at_ms,
                    &plans,
                )?;
                if replacements.len() != prepared.len() {
                    return Err(calyx_write_failed_detail(
                        "calyx_temporal_metadata_backfill",
                        format!(
                            "registry batch resolver returned {} rows for {} prepared constellations",
                            replacements.len(),
                            prepared.len()
                        ),
                    ));
                }
                for (row_index, (row, resolved)) in
                    prepared.iter_mut().zip(replacements).enumerate()
                {
                    for (slot_id, vector) in resolved {
                        let prior = row.constellation.slots.insert(slot_id, vector);
                        if !prior
                            .as_ref()
                            .is_some_and(constellations::is_deferred_measurement_placeholder)
                        {
                            return Err(calyx_write_failed_detail(
                                "calyx_temporal_metadata_backfill",
                                format!(
                                    "registry batch replacement did not find its deferred slot: row_index={row_index} slot_id={} prior={prior:?}",
                                    slot_id.0
                                ),
                            ));
                        }
                    }
                }

                let expected_ids = prepared
                    .iter()
                    .map(|row| row.constellation.cx_id)
                    .collect::<Vec<_>>();
                let mut temporal_batches = Vec::<(
                    Vec<usize>,
                    calyx_aster::vault::TemporalMetadataBackfill,
                )>::new();
                let mut temporal_batch_by_id = BTreeMap::<CxId, usize>::new();
                for (row_index, row) in prepared.iter().enumerate() {
                    let (identity, temporal) =
                        constellations::temporal_migration_metadata(&row.constellation);
                    if !constellations::temporal_migration_eligible(&temporal)? {
                        continue;
                    }
                    let request = calyx_aster::vault::TemporalMetadataBackfill {
                        cx_id: row.constellation.cx_id,
                        expected_panel_version: row.constellation.panel_version,
                        expected_identity: identity,
                        expected_temporal: temporal,
                    };
                    if let Some(batch_index) = temporal_batch_by_id.get(&request.cx_id).copied() {
                        if temporal_batches[batch_index].1 != request {
                            return Err(calyx_write_failed_detail(
                                "calyx_temporal_metadata_backfill",
                                format!(
                                    "duplicate content identity has conflicting authoritative temporal metadata in one page: cx_id={} first_row_index={} conflicting_row_index={row_index}",
                                    request.cx_id, temporal_batches[batch_index].0[0]
                                ),
                            ));
                        }
                        temporal_batches[batch_index].0.push(row_index);
                    } else {
                        temporal_batch_by_id.insert(request.cx_id, temporal_batches.len());
                        temporal_batches.push((vec![row_index], request));
                    }
                }

                let mut sidecars = Vec::with_capacity(prepared.len());
                let constellations = prepared
                    .into_iter()
                    .map(|row| {
                        sidecars.push(TemporalBackfillRowSidecars {
                            transcript_outcome: row.transcript_outcome,
                            action_outcome_present: row.action_outcome_present,
                        });
                        row.constellation
                    })
                    .collect::<Vec<_>>();
                let puts = vault
                    .put_observation_constellation_batch(constellations)
                    .map_err(|error| {
                        calyx_write_failed(
                            "calyx_temporal_metadata_backfill",
                            &format!(
                                "materialize ordered Calyx constellation page batch rows={} wal_max_record_bytes={}",
                                expected_ids.len(),
                                wal::MAX_RECORD_BYTES
                            ),
                            &error,
                        )
                    })?;
                if puts.len() != expected_ids.len() {
                    return Err(calyx_write_failed_detail(
                        "calyx_temporal_metadata_backfill",
                        format!(
                            "constellation page batch returned {} readbacks for {} ordered inputs",
                            puts.len(),
                            expected_ids.len()
                        ),
                    ));
                }
                for (row_index, (put, expected_id)) in
                    puts.iter().zip(&expected_ids).enumerate()
                {
                    if put.cx_id != expected_id.to_string() {
                        return Err(calyx_write_failed_detail(
                            "calyx_temporal_metadata_backfill",
                            format!(
                                "constellation page batch readback identity mismatch at row_index={row_index}: expected_cx_id={expected_id} actual_cx_id={}",
                                put.cx_id
                            ),
                        ));
                    }
                }

                let migration_requests = temporal_batches
                    .iter()
                    .map(|(_row_indexes, request)| request.clone())
                    .collect::<Vec<_>>();
                let migration_results = vault
                    .backfill_temporal_metadata_batch(migration_requests)
                    .map_err(|error| {
                        calyx_write_failed(
                            "calyx_temporal_metadata_backfill",
                            &format!(
                                "batch temporal Base rewrites rows={} page_rows={}",
                                temporal_batches.len(),
                                expected_ids.len()
                            ),
                            &error,
                        )
                    })?;
                if migration_results.len() != temporal_batches.len() {
                    return Err(calyx_write_failed_detail(
                        "calyx_temporal_metadata_backfill",
                        format!(
                            "temporal metadata batch returned {} outcomes for {} unique eligible identities",
                            migration_results.len(),
                            temporal_batches.len()
                        ),
                    ));
                }
                let mut migrations = vec![None; expected_ids.len()];
                for ((row_indexes, _request), migration) in
                    temporal_batches.into_iter().zip(migration_results)
                {
                    for row_index in row_indexes {
                        migrations[row_index] = Some(migration.clone());
                    }
                }

                let outcomes = puts
                    .into_iter()
                    .zip(expected_ids)
                    .zip(sidecars)
                    .enumerate()
                    .map(|(row_index, ((put, cx_id), sidecars))| TemporalBackfillRowOutcome {
                        put,
                        migration: migrations[row_index].take(),
                        cx_id,
                        sidecars,
                    })
                    .collect::<Vec<_>>();
                Ok((outcomes, committed_row_indexes, build_failures))
            },
        )?;
        row_failures.extend(build_failures);
        if page_outcomes.len() != committed_row_indexes.len() {
            return Err(calyx_write_failed_detail(
                "calyx_temporal_metadata_backfill",
                format!(
                    "backfill page produced {} ordered outcomes for {} committed source rows",
                    page_outcomes.len(),
                    committed_row_indexes.len()
                ),
            ));
        }
        let committed_row_indexes = committed_row_indexes.into_iter().collect::<BTreeSet<_>>();
        let rows = rows
            .into_iter()
            .enumerate()
            .filter_map(|(row_index, row)| {
                committed_row_indexes.contains(&row_index).then_some(row)
            })
            .collect::<Vec<_>>();

        let mut carry_targets = Vec::with_capacity(rows.len());
        let mut transcript_anchor_sources = Vec::new();
        let mut row_reports = Vec::with_capacity(rows.len());
        for ((key, raw), outcome) in rows.into_iter().zip(page_outcomes) {
            let row_index = row_reports.len();
            let mut row_report = constellations::TemporalMetadataBackfillRowReport {
                source_key: key.clone(),
                inserted_rows: 0,
                backfilled_rows: 0,
                already_current_rows: 0,
                temporal_ineligible_rows: 0,
                outcome_anchored_rows: 0,
                outcome_absent_rows: 0,
                outcome_unadjudicable_rows: 0,
                anchors_carried_forward: 0,
                rows_anchor_carried: 0,
                anchor_carry_source_generations_read: 0,
                latest_seq: 0,
            };
            if outcome.migration.is_none() {
                temporal_ineligible_rows = temporal_ineligible_rows.saturating_add(1);
                row_report.temporal_ineligible_rows = 1;
            }
            if outcome.put.disposition.inserted() {
                inserted_rows = inserted_rows.saturating_add(1);
                row_report.inserted_rows = 1;
            } else if outcome
                .migration
                .as_ref()
                .is_some_and(calyx_aster::vault::TemporalMetadataMigration::changed)
            {
                backfilled_rows = backfilled_rows.saturating_add(1);
                row_report.backfilled_rows = 1;
            } else if outcome.migration.is_some() {
                already_current_rows = already_current_rows.saturating_add(1);
                row_report.already_current_rows = 1;
            }

            if let Some(transcript) = outcome.sidecars.transcript_outcome {
                if transcript.unadjudicable {
                    outcome_unadjudicable_rows = outcome_unadjudicable_rows.saturating_add(1);
                    row_report.outcome_unadjudicable_rows = 1;
                }
                if let Some(anchor) = transcript.anchor {
                    row_report.outcome_anchored_rows = 1;
                    transcript_anchor_sources.push(GroundingAnchorSource {
                        source_cf: cf::CF_AGENT_TRANSCRIPTS,
                        source_key: key.clone(),
                        raw_bytes: raw.clone(),
                        anchor,
                    });
                } else {
                    outcome_absent_rows = outcome_absent_rows.saturating_add(1);
                    row_report.outcome_absent_rows = 1;
                }
            }
            if let Some(action_outcome_present) = outcome.sidecars.action_outcome_present {
                if action_outcome_present {
                    outcome_anchored_rows = outcome_anchored_rows.saturating_add(1);
                    row_report.outcome_anchored_rows = 1;
                } else {
                    outcome_absent_rows = outcome_absent_rows.saturating_add(1);
                    row_report.outcome_absent_rows = 1;
                }
            }
            if carry_superseded_anchors {
                row_report.anchor_carry_source_generations_read = superseded_generation_count;
                carry_targets.push(AnchorCarryBatchTarget {
                    row_index,
                    source_key: key,
                    active_cx_id: outcome.cx_id,
                    prior_anchors: anchor_lineage
                        .get(&constellations::hex_encode(&row_report.source_key))
                        .cloned()
                        .unwrap_or_default(),
                });
            }
            row_reports.push(row_report);
        }

        // #2121: transcript rows used to perform one pre-read and up to one
        // ledger/WAL/fsync commit each. The native multi-Cx anchor API validates
        // every target at one snapshot, commits the missing set atomically, and
        // the storage wrapper independently reads every physical Anchors-CF row.
        if !transcript_anchor_sources.is_empty() {
            let requested = transcript_anchor_sources.len() as u64;
            let source_evidence = transcript_anchor_sources
                .iter()
                .map(|source| {
                    serde_json::json!({
                        "source_key_sha256": constellations::sha256_hex(&source.source_key),
                        "source_value_sha256": constellations::sha256_hex(&source.raw_bytes),
                    })
                })
                .collect::<Vec<_>>();
            let payload = serde_json::json!({
                "mode": "agent-transcript-outcome-backfill-batch",
                "source_cf": cf::CF_AGENT_TRANSCRIPTS,
                "source_row_count": requested,
                "source_evidence": source_evidence,
            });
            let report = <Self as StorageBackend>::put_grounding_anchors_for_sources(
                self,
                transcript_anchor_sources,
                &payload,
            )?;
            if report.requested_anchor_count != requested
                || report.readback_exact_match_count != requested
            {
                return Err(calyx_write_failed_detail(
                    "calyx_temporal_metadata_backfill",
                    format!(
                        "transcript outcome batch readback mismatch: expected={requested} requested={} physical_exact_matches={}",
                        report.requested_anchor_count, report.readback_exact_match_count
                    ),
                ));
            }
            outcome_anchored_rows = outcome_anchored_rows.saturating_add(requested);
        }

        // Carry only after every active generation row and fresh outcome in the
        // page is durable. Every eligible target is prepared and independently
        // read back, but the missing anchors share one ledger/WAL/fsync commit.
        // There is no retry-to-single-row path: one invalid batch fails with the
        // complete target set still attributable in the error.
        if !carry_targets.is_empty() {
            anchor_carry_source_generations_read =
                superseded_generation_count.saturating_mul(carry_targets.len() as u64);
            let carried = self.with_vault(
                "calyx_anchor_carry_forward",
                "batch-carry grounded anchors from indexed superseded Base rows",
                true,
                |vault| carry_forward_grounded_anchors_batch(vault, source_cf, carry_targets),
            )?;
            for (row_index, outcome) in carried {
                let source_row_count = row_reports.len();
                let row_report = row_reports.get_mut(row_index).ok_or_else(|| {
                    calyx_write_failed_detail(
                        "calyx_anchor_carry_forward",
                        format!(
                            "anchor carry batch returned out-of-range row_index={row_index} for {source_row_count} source rows"
                        ),
                    )
                })?;
                row_report.anchors_carried_forward = outcome.anchors_written;
                anchors_carried_forward =
                    anchors_carried_forward.saturating_add(outcome.anchors_written);
                if outcome.anchors_written > 0 {
                    row_report.rows_anchor_carried = 1;
                    rows_anchor_carried = rows_anchor_carried.saturating_add(1);
                }
            }
        }
        let latest_seq = self.with_vault(
            "calyx_temporal_metadata_backfill",
            "read Calyx temporal metadata backfill sequence",
            false,
            |vault| Ok(vault.latest_seq()),
        )?;
        for row_report in &mut row_reports {
            row_report.latest_seq = latest_seq;
        }
        let report = constellations::TemporalMetadataBackfillReport {
            source_cf: source_cf.to_owned(),
            examined_rows,
            inserted_rows,
            backfilled_rows,
            already_current_rows,
            temporal_ineligible_rows,
            outcome_anchored_rows,
            outcome_absent_rows,
            outcome_unadjudicable_rows,
            anchors_carried_forward,
            rows_anchor_carried,
            anchor_carry_source_generations_read,
            candidate_rows_examined: candidate_rows_examined as u64,
            expired_rows_skipped: expired_rows_skipped as u64,
            latest_seq,
            resume_after_physical,
            more,
            row_reports,
            row_failures,
        };
        if (!pointwise_preflight && source_keys.is_some()) || (source_keys.is_none() && !more) {
            self.release_anchor_carry_lineage(Some(source_cf));
        }
        Ok(report)
    }

    fn release_temporal_backfill_lineage(&self, source_cf: Option<&str>) -> usize {
        self.release_anchor_carry_lineage(source_cf)
    }

    #[allow(clippy::too_many_lines)]
    fn put_timeline_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &TimelineRecord,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put timeline Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_TIMELINE_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_TIMELINE)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let trigger_cx_id = context.cx_id;
                let constellation = constellations::build_timeline_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback = put_observation_with_lifecycle(
                    vault,
                    SYN_TIMELINE_PANEL_NAME,
                    raw_bytes,
                    raw_bytes,
                    constellation,
                )?;
                if let Some(app) = record
                    .app
                    .as_deref()
                    .map(str::trim)
                    .filter(|app| !app.is_empty())
                {
                    let recurrence_context = serde_json::to_vec(&serde_json::json!({
                        "source_cf": cf::CF_TIMELINE,
                        "source_key_hex": constellations::hex_encode(source_key),
                    }))
                    .map_err(|error| StorageError::WriteFailed {
                        cf_name: "calyx_recurrence".to_owned(),
                        detail: format!(
                            "encode app-usage recurrence context for source key {}: {error}",
                            constellations::hex_encode(source_key)
                        ),
                    })?;
                    let recurrence = put_recurrence_subject_occurrence_on_vault(
                        vault,
                        RecurrenceSubjectKind::AppUsage,
                        app,
                        record.ts_ns,
                        source_key,
                        &recurrence_context,
                        Some(trigger_cx_id),
                    )?;
                    tracing::debug!(
                        code = "CALYX_APP_USAGE_RECURRENCE_APPENDED",
                        subject_cx_id = %recurrence.subject_cx_id,
                        subject_id = %recurrence.subject_id,
                        occurrence_id = recurrence.occurrence.occurrence_id,
                        disposition = ?recurrence.occurrence.disposition,
                        frequency = recurrence.occurrence.frequency,
                        source_key_hex = %constellations::hex_encode(source_key),
                        "timeline app usage projected into native Calyx recurrence series"
                    );
                }
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_TIMELINE_PANEL_NAME,
                    panel_version: SYN_TIMELINE_PANEL_VERSION,
                    source_cf: cf::CF_TIMELINE,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_TIMELINE_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "timeline row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_TIMELINE_PANEL_NAME,
                    cf::CF_TIMELINE,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_episode_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &EpisodeRecord,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put episode Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_EPISODE_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_EPISODES)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_episode_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback = put_observation_with_lifecycle(
                    vault,
                    SYN_EPISODE_PANEL_NAME,
                    raw_bytes,
                    raw_bytes,
                    constellation,
                )?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_EPISODE_PANEL_NAME,
                    panel_version: SYN_EPISODE_PANEL_VERSION,
                    source_cf: cf::CF_EPISODES,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_EPISODE_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "episode row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_EPISODE_PANEL_NAME,
                    cf::CF_EPISODES,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_episode_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, EpisodeRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put episode Calyx constellation batch",
            true,
            |vault| {
                let batch = build_episode_constellation_batch(vault, rows)?;
                let readbacks = vault
                    .put_observation_constellation_batch(batch.constellations)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_constellation",
                            "put episode observation constellation batch",
                            &source,
                        )
                    })?;
                constellation_batch_reports(
                    batch.pending_reports,
                    readbacks,
                    SYN_EPISODE_PANEL_NAME,
                    SYN_EPISODE_PANEL_VERSION,
                    cf::CF_EPISODES,
                    constellations::duration_us(started.elapsed()),
                )
            },
        );
        match result {
            Ok(reports) => {
                for report in &reports {
                    constellations::emit_success_metric(report);
                }
                let inserted = reports.iter().filter(|report| report.inserted()).count();
                let deduped = reports.iter().filter(|report| report.deduped()).count();
                tracing::debug!(
                    code = "CALYX_EPISODE_CONSTELLATION_BATCH_PUT",
                    panel_name = SYN_EPISODE_PANEL_NAME,
                    panel_version = SYN_EPISODE_PANEL_VERSION,
                    source_cf = cf::CF_EPISODES,
                    input_rows = rows.len(),
                    inserted,
                    deduped,
                    duration_us = constellations::duration_us(started.elapsed()),
                    "episode rows measured into native Calyx constellations as one batch"
                );
                Ok(reports)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_EPISODE_PANEL_NAME,
                    cf::CF_EPISODES,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_agent_event_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &AgentEventRecord,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put agent event Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_AGENT_EVENT_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_AGENT_EVENTS)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_agent_event_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put agent event observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_AGENT_EVENT_PANEL_NAME,
                    panel_version: SYN_AGENT_EVENT_PANEL_VERSION,
                    source_cf: cf::CF_AGENT_EVENTS,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_AGENT_EVENT_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "agent event row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_AGENT_EVENT_PANEL_NAME,
                    cf::CF_AGENT_EVENTS,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_agent_event_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, AgentEventRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put agent event Calyx constellation batch",
            true,
            |vault| {
                let batch = build_agent_event_constellation_batch(vault, rows)?;
                let readbacks = vault
                    .put_observation_constellation_batch(batch.constellations)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_constellation",
                            "put agent event observation constellation batch",
                            &source,
                        )
                    })?;
                constellation_batch_reports(
                    batch.pending_reports,
                    readbacks,
                    SYN_AGENT_EVENT_PANEL_NAME,
                    SYN_AGENT_EVENT_PANEL_VERSION,
                    cf::CF_AGENT_EVENTS,
                    constellations::duration_us(started.elapsed()),
                )
            },
        );
        emit_constellation_batch_result(
            result,
            rows.len(),
            SYN_AGENT_EVENT_PANEL_NAME,
            SYN_AGENT_EVENT_PANEL_VERSION,
            cf::CF_AGENT_EVENTS,
            "CALYX_AGENT_EVENT_CONSTELLATION_BATCH_PUT",
            started,
        )
    }

    fn put_agent_transcript_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &AgentTranscriptRecord,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put agent transcript Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_AGENT_TRANSCRIPT_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_AGENT_TRANSCRIPTS)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_agent_transcript_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put agent transcript observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_AGENT_TRANSCRIPT_PANEL_NAME,
                    panel_version: SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
                    source_cf: cf::CF_AGENT_TRANSCRIPTS,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_AGENT_TRANSCRIPT_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "agent transcript row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_AGENT_TRANSCRIPT_PANEL_NAME,
                    cf::CF_AGENT_TRANSCRIPTS,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_agent_transcript_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, AgentTranscriptRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put agent transcript Calyx constellation batch",
            true,
            |vault| {
                let batch = build_agent_transcript_constellation_batch(vault, rows)?;
                let readbacks = vault
                    .put_observation_constellation_batch(batch.constellations)
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_constellation",
                            "put agent transcript observation constellation batch",
                            &source,
                        )
                    })?;
                constellation_batch_reports(
                    batch.pending_reports,
                    readbacks,
                    SYN_AGENT_TRANSCRIPT_PANEL_NAME,
                    SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
                    cf::CF_AGENT_TRANSCRIPTS,
                    constellations::duration_us(started.elapsed()),
                )
            },
        );
        emit_constellation_batch_result(
            result,
            rows.len(),
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
            cf::CF_AGENT_TRANSCRIPTS,
            "CALYX_AGENT_TRANSCRIPT_CONSTELLATION_BATCH_PUT",
            started,
        )
    }

    fn put_action_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put action Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_ACTION_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_ACTION_LOG)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_action_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put action observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_ACTION_PANEL_NAME,
                    panel_version: SYN_ACTION_PANEL_VERSION,
                    source_cf: cf::CF_ACTION_LOG,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_ACTION_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "action row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_ACTION_PANEL_NAME,
                    cf::CF_ACTION_LOG,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_reflex_audit_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &StoredReflexAudit,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put reflex audit Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_REFLEX_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_REFLEX_AUDIT)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_reflex_audit_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put reflex audit observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_REFLEX_PANEL_NAME,
                    panel_version: SYN_REFLEX_PANEL_VERSION,
                    source_cf: cf::CF_REFLEX_AUDIT,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_REFLEX_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "reflex audit row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_REFLEX_PANEL_NAME,
                    cf::CF_REFLEX_AUDIT,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_reflex_lifecycle_grounded_publication(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &StoredReflexAudit,
    ) -> StorageResult<ReflexRegistrationPublicationReport> {
        const OPERATION_CF: &str = "calyx_reflex_lifecycle_publication";
        let started = Instant::now();
        validate_reflex_registration_publication(&guards, &batches, source_key, raw_bytes, record)?;

        let result = self.with_vault(
            OPERATION_CF,
            "atomically publish revision-guarded reflex registration",
            true,
            |vault| {
                let prepared = prepare_reflex_registration_publication(
                    vault, guards, batches, source_key, raw_bytes, record,
                )?;
                commit_reflex_registration_publication(
                    vault, prepared, source_key, raw_bytes, started,
                )
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report.constellation);
                tracing::info!(
                    code = "CALYX_REFLEX_LIFECYCLE_ATOMIC_PUBLICATION_COMMITTED",
                    reflex_id = %record.reflex_id,
                    audit_id = %record.audit_id,
                    committed_seq = report.committed_seq,
                    source_row_count = report.source_row_count,
                    source_readback_exact_match_count = report.source_readback_exact_match_count,
                    cx_id = %report.constellation.cx_id,
                    ledger_seq = report.anchor.ledger_seq,
                    duration_us = report.constellation.duration_us,
                    "reflex lifecycle source, projections, desired state, constellation, anchor, and ledger entry committed atomically with physical readback"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_REFLEX_PANEL_NAME,
                    cf::CF_REFLEX_AUDIT,
                    error.code(),
                    started.elapsed(),
                );
                tracing::error!(
                    code = "REFLEX_LIFECYCLE_ATOMIC_PUBLICATION_FAILED",
                    reflex_id = %record.reflex_id,
                    audit_id = %record.audit_id,
                    error_code = error.code(),
                    detail = %error,
                    "reflex lifecycle atomic durable publication failed before scheduler publication"
                );
                Err(error)
            }
        }
    }

    fn put_reflex_lifecycle_grounded_batch_publication(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<OwnedCfWriteBatch>,
        members: Vec<ReflexGroundedLifecycleMember>,
    ) -> StorageResult<ReflexLifecycleBatchPublicationReport> {
        Self::put_reflex_lifecycle_grounded_batch_publication_inner(self, guards, batches, &members)
    }

    fn put_process_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put process Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_PROCESS_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_PROCESS_HISTORY)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_process_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put process observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_PROCESS_PANEL_NAME,
                    panel_version: SYN_PROCESS_PANEL_VERSION,
                    source_cf: cf::CF_PROCESS_HISTORY,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_PROCESS_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "process row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_PROCESS_PANEL_NAME,
                    cf::CF_PROCESS_HISTORY,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_sampled_observation_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &StoredObservation,
    ) -> StorageResult<Option<ConstellationPutReport>> {
        let started = Instant::now();
        let sample_permits =
            match constellations::observation_constellation_sample_permits(source_key) {
                Ok(sample_permits) => sample_permits,
                Err(error) => {
                    constellations::emit_error_metric(
                        SYN_OBSERVATION_PANEL_NAME,
                        cf::CF_OBSERVATIONS,
                        error.code(),
                        started.elapsed(),
                    );
                    return Err(error);
                }
            };
        if !sample_permits {
            tracing::debug!(
                code = "CALYX_OBSERVATION_CONSTELLATION_SAMPLE_SKIPPED",
                source_cf = cf::CF_OBSERVATIONS,
                source_key_hex = %constellations::hex_encode(source_key),
                sampler = "ts_ns_seq_modulo_config",
                "observation row skipped by deterministic Calyx constellation sampler"
            );
            return Ok(None);
        }
        let result = self.with_vault(
            "calyx_constellation",
            "put sampled observation Calyx constellation",
            true,
            |vault| {
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(raw_bytes, SYN_OBSERVATION_PANEL_VERSION),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_OBSERVATIONS)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_observation_constellation(
                    context, source_key, raw_bytes, record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put sampled observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_OBSERVATION_PANEL_NAME,
                    panel_version: SYN_OBSERVATION_PANEL_VERSION,
                    source_cf: cf::CF_OBSERVATIONS,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_OBSERVATION_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "sampled observation row measured into native Calyx constellation"
                );
                Ok(Some(report))
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_OBSERVATION_PANEL_NAME,
                    cf::CF_OBSERVATIONS,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_outcome_constellation(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put outcome Calyx constellation",
            true,
            |vault| {
                let panel = constellations::anchor_panel_for_source_row(source_cf, source_key)?;
                if panel.panel_name != SYN_OUTCOME_PANEL_NAME
                    || panel.panel_version != SYN_OUTCOME_PANEL_VERSION
                {
                    return Err(calyx_write_failed_detail(
                        "calyx_constellation",
                        format!(
                            "put_outcome_constellation requires an outcome-panel source CF; {source_cf} maps to {} version {}",
                            panel.panel_name, panel.panel_version
                        ),
                    ));
                }
                let input_bytes = constellations::source_constellation_input_bytes(
                    panel.input_mode,
                    source_cf,
                    source_key,
                    raw_bytes,
                );
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(&input_bytes, panel.panel_version),
                    created_at_ms: calyx_clock_now_for_write(vault, source_cf)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_outcome_constellation(
                    context,
                    source_cf,
                    source_key,
                    raw_bytes,
                    &input_bytes,
                    record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put outcome observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_OUTCOME_PANEL_NAME,
                    panel_version: SYN_OUTCOME_PANEL_VERSION,
                    source_cf,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_OUTCOME_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "outcome row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_OUTCOME_PANEL_NAME,
                    source_cf,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_mcp_usage_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
    ) -> StorageResult<ConstellationPutReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_constellation",
            "put MCP usage Calyx constellation",
            true,
            |vault| {
                let panel = constellations::anchor_panel_for_source_row(cf::CF_KV, source_key)?;
                if panel.panel_name != SYN_MCP_USAGE_PANEL_NAME
                    || panel.panel_version != SYN_MCP_USAGE_PANEL_VERSION
                {
                    return Err(calyx_write_failed_detail(
                        "calyx_constellation",
                        format!(
                            "put_mcp_usage_constellation requires an MCP usage source row; key_hex={} maps to {} version {}",
                            constellations::hex_encode(source_key),
                            panel.panel_name,
                            panel.panel_version
                        ),
                    ));
                }
                let input_bytes = constellations::source_constellation_input_bytes(
                    panel.input_mode,
                    cf::CF_KV,
                    source_key,
                    raw_bytes,
                );
                let context = NativeConstellationContext {
                    vault_id: vault.vault_id_value(),
                    cx_id: vault.cx_id_for_input(&input_bytes, panel.panel_version),
                    created_at_ms: calyx_clock_now_for_write(vault, cf::CF_KV)?,
                    next_ledger_seq: vault.latest_seq().saturating_add(1),
                };
                let constellation = constellations::build_mcp_usage_constellation(
                    context,
                    source_key,
                    raw_bytes,
                    &input_bytes,
                    record,
                )?;
                let slot_count = constellation.slots.len() as u64;
                let scalar_count = constellation.scalars.len() as u64;
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put MCP usage observation constellation",
                                &source,
                            )
                        })?;
                Ok(constellation_report(ConstellationReportInput {
                    panel_name: SYN_MCP_USAGE_PANEL_NAME,
                    panel_version: SYN_MCP_USAGE_PANEL_VERSION,
                    source_cf: cf::CF_KV,
                    source_key,
                    raw_bytes,
                    readback,
                    slot_count,
                    scalar_count,
                    duration_us: constellations::duration_us(started.elapsed()),
                }))
            },
        );
        match result {
            Ok(report) => {
                constellations::emit_success_metric(&report);
                tracing::debug!(
                    code = "CALYX_MCP_USAGE_CONSTELLATION_PUT",
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    raw_sha256 = %report.raw_sha256,
                    cx_id = %report.cx_id,
                    disposition = report.disposition.as_str(),
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "MCP usage row measured into native Calyx constellation"
                );
                Ok(report)
            }
            Err(error) => {
                constellations::emit_error_metric(
                    SYN_MCP_USAGE_PANEL_NAME,
                    cf::CF_KV,
                    error.code(),
                    started.elapsed(),
                );
                Err(error)
            }
        }
    }

    fn put_mcp_usage_grounded_publication(
        &self,
        source_rows: Vec<RawRow>,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &Value,
        anchor: GroundingAnchor,
        ledger_payload: &Value,
    ) -> StorageResult<McpUsageGroundedPublicationReport> {
        let started = Instant::now();
        validate_mcp_usage_source_rows(&source_rows, source_key, raw_bytes)?;
        self.with_vault(
            "calyx_mcp_usage_publication",
            "atomically publish grounded MCP usage",
            true,
            |vault| {
                let prepared = prepare_mcp_usage_grounded_publication(
                    vault,
                    source_rows,
                    source_key,
                    raw_bytes,
                    record,
                    anchor,
                    ledger_payload,
                )?;
                commit_mcp_usage_grounded_publication(
                    vault, prepared, source_key, raw_bytes, started,
                )
            },
        )
    }

    fn put_grounding_anchor_for_source(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
        anchor: GroundingAnchor,
        ledger_payload: &Value,
    ) -> StorageResult<CalyxAnchorWriteReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_anchors",
            "put grounded Calyx anchor",
            true,
            |vault| {
                let panel = constellations::anchor_panel_for_source_row(source_cf, source_key)?;
                let input_bytes = constellations::source_constellation_input_bytes(
                    panel.input_mode,
                    source_cf,
                    source_key,
                    raw_bytes,
                );
                let cx_id = vault.cx_id_for_input(&input_bytes, panel.panel_version);
                let calyx_anchor = grounding_anchor_to_calyx(anchor)?;
                let payload = serde_json::to_vec(ledger_payload).map_err(|source| {
                    StorageError::EncodeJson {
                        type_name: "calyx_grounding_anchor_ledger_payload",
                        source,
                    }
                })?;
                let write =
                    vault
                        .put_grounding_anchors(
                            cx_id,
                            vec![calyx_anchor.clone()],
                            payload,
                            "synapse-outcome-anchors",
                        )
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_anchors",
                                "put ledger-stamped grounded anchor",
                                &source,
                            )
                        })?;
                let rows = vault.scan_anchors_for_cx(cx_id).map_err(|source| {
                    calyx_read_failed("calyx_anchors", "read back grounded anchors", &source)
                })?;
                let exact_matches = rows
                    .iter()
                    .filter(|row| row.anchor == calyx_anchor)
                    .count();
                if exact_matches != 1 {
                    return Err(calyx_write_failed_detail(
                        "calyx_anchors",
                        format!(
                            "grounded anchor readback mismatch for cx_id={cx_id}: expected exactly 1 matching anchor, found {exact_matches}; total anchors={}",
                            rows.len()
                        ),
                    ));
                }
                Ok(anchor_write_report(AnchorWriteReportInput {
                    source_cf,
                    source_key,
                    raw_bytes,
                    panel_name: panel.panel_name,
                    panel_version: panel.panel_version,
                    cx_id,
                    anchor: &calyx_anchor,
                    write,
                    readback_anchor_count: rows.len(),
                }))
            },
        );
        match result {
            Ok(report) => {
                tracing::info!(
                    code = "CALYX_GROUNDING_ANCHOR_PUT",
                    source_cf = report.source_cf,
                    source_key_hex = %report.source_key_hex,
                    panel_name = report.panel_name,
                    panel_version = report.panel_version,
                    cx_id = %report.cx_id,
                    anchor_kind = %report.anchor_kind,
                    anchor_source = %report.anchor_source,
                    confidence = report.confidence,
                    ledger_seq = report.ledger_seq,
                    latest_seq = report.latest_seq,
                    duration_us = constellations::duration_us(started.elapsed()),
                    "grounded anchor written with physical Anchors CF readback"
                );
                Ok(report)
            }
            Err(error) => {
                tracing::error!(
                    code = "CALYX_GROUNDING_ANCHOR_FAILED",
                    source_cf,
                    source_key_hex = %constellations::hex_encode(source_key),
                    raw_sha256 = %constellations::sha256_hex(raw_bytes),
                    detail = %error,
                    duration_us = constellations::duration_us(started.elapsed()),
                    "grounded anchor write/readback failed"
                );
                Err(error)
            }
        }
    }

    fn put_grounding_anchors_for_sources(
        &self,
        sources: Vec<GroundingAnchorSource>,
        ledger_payload: &Value,
    ) -> StorageResult<CalyxAnchorBatchWriteReport> {
        let started = Instant::now();
        let result = self.with_vault(
            "calyx_anchors",
            "put grounded Calyx anchor batch",
            true,
            |vault| {
                if sources.is_empty() {
                    return Err(calyx_write_failed_detail(
                        "calyx_anchors",
                        "grounded anchor batch must contain at least one source row",
                    ));
                }
                let payload = serde_json::to_vec(ledger_payload).map_err(|source| {
                    StorageError::EncodeJson {
                        type_name: "calyx_grounding_anchor_batch_ledger_payload",
                        source,
                    }
                })?;
                let (prepared, calyx_entries) = prepare_grounding_anchor_sources(vault, sources)?;
                let write = match vault.put_grounding_anchors_for_many(
                    calyx_entries,
                    payload,
                    "synapse-outcome-anchors",
                ) {
                    Ok(write) => write,
                    Err(source) => {
                        let error = calyx_write_failed(
                            "calyx_anchors",
                            "put multi-constellation ledger-stamped grounded anchors",
                            &source,
                        );
                        // #2072: a conflict leaves the journal committed and the
                        // anchor not advanced, which is the silent un-grounding
                        // #1980/#1984 exist to stop, arriving through a different
                        // door. Name the exact constellations in ONE record so a
                        // repair can find them, instead of leaving 106 rows
                        // carrying a stale value with nothing scheduled.
                        if error.code() == CALYX_ANCHOR_VALUE_CONFLICT_CODE {
                            record_anchor_conflict_identities(&prepared, &error);
                        }
                        return Err(error);
                    }
                };
                let readback_exact_match_count = readback_grounding_anchor_batch(vault, &prepared)?;
                Ok(anchor_batch_write_report(
                    write,
                    readback_exact_match_count,
                    constellations::duration_us(started.elapsed()),
                ))
            },
        );
        match result {
            Ok(report) => {
                tracing::info!(
                    code = "CALYX_GROUNDING_ANCHOR_BATCH_PUT",
                    requested_anchor_count = report.requested_anchor_count,
                    written_anchor_count = report.written_anchor_count,
                    existing_anchor_count = report.existing_anchor_count,
                    readback_exact_match_count = report.readback_exact_match_count,
                    ledger_seq = ?report.ledger_seq,
                    latest_seq = report.latest_seq,
                    duration_us = report.duration_us,
                    "grounded anchor batch written with physical Anchors CF readback"
                );
                Ok(report)
            }
            Err(error) => {
                tracing::error!(
                    code = "CALYX_GROUNDING_ANCHOR_BATCH_FAILED",
                    detail = %error,
                    duration_us = constellations::duration_us(started.elapsed()),
                    "grounded anchor batch write/readback failed"
                );
                Err(error)
            }
        }
    }

    fn calyx_anchor_scan_for_source(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
    ) -> StorageResult<CalyxAnchorScanReport> {
        self.with_vault(
            "calyx_anchors",
            "scan grounded Calyx anchors",
            false,
            |vault| {
                let panel = constellations::anchor_panel_for_source_row(source_cf, source_key)?;
                let input_bytes = constellations::source_constellation_input_bytes(
                    panel.input_mode,
                    source_cf,
                    source_key,
                    raw_bytes,
                );
                let cx_id = vault.cx_id_for_input(&input_bytes, panel.panel_version);
                let rows = vault.scan_anchors_for_cx(cx_id).map_err(|source| {
                    calyx_read_failed("calyx_anchors", "scan grounded anchors", &source)
                })?;
                Ok(CalyxAnchorScanReport {
                    source_cf: source_cf.to_owned(),
                    source_key_hex: constellations::hex_encode(source_key),
                    source_value_sha256: constellations::sha256_hex(raw_bytes),
                    panel_name: panel.panel_name.to_owned(),
                    panel_version: panel.panel_version,
                    cx_id: cx_id.to_string(),
                    anchors: anchor_rows(cx_id, rows),
                })
            },
        )
    }

    fn run_pressure_check_once(
        &self,
        storage_path: &Path,
    ) -> StorageResult<pressure::PressureReport> {
        pressure::run_once(
            &self.pressure,
            storage_path,
            &pressure::PressureConfig::default(),
            &CalyxPressureCompaction::new(Arc::clone(&self.vault)),
        )
    }

    fn run_pressure_check_with_free_bytes_sample(
        &self,
        free_bytes: u64,
    ) -> StorageResult<pressure::PressureReport> {
        pressure::run_once_with_free_bytes(
            &self.pressure,
            &pressure::PressureConfig::default(),
            free_bytes,
            &CalyxPressureCompaction::new(Arc::clone(&self.vault)),
        )
    }

    fn spawn_pressure_task(&self, storage_path: &Path) -> StorageResult<pressure::PressureTask> {
        pressure::spawn(
            Arc::clone(&self.pressure),
            storage_path.to_path_buf(),
            pressure::PressureConfig::default(),
            Arc::new(CalyxPressureCompaction::new(Arc::clone(&self.vault))),
        )
    }

    fn scan_cf(&self, cf_name: &str) -> StorageResult<Vec<RawRow>> {
        self.read_all_rows(cf_name)
    }

    fn scan_cf_prefix(&self, cf_name: &str, prefix: &[u8]) -> StorageResult<Vec<RawRow>> {
        self.scan_cf_prefix_from(cf_name, prefix, prefix)
    }

    fn scan_cf_prefix_from(
        &self,
        cf_name: &str,
        prefix: &[u8],
        start_key: &[u8],
    ) -> StorageResult<Vec<RawRow>> {
        self.with_vault(
            cf_name,
            "scan ordered Calyx logical prefix range",
            false,
            |vault| {
                let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
                let range = calyx_ordered_prefix_from_range(collection_id, prefix, start_key)?;
                range.map_or_else(
                    || Ok(Vec::new()),
                    |range| read_rows_from_vault_range_filtered(vault, cf_name, &range, false),
                )
            },
        )
    }

    fn scan_cf_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        if max_rows == 0 {
            return Ok((Vec::new(), false));
        }
        if max_rows > LATEST_CF_RANGE_PAGE_MAX_ROWS {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "ordered Calyx logical page max_rows {max_rows} exceeds the hard candidate ceiling {LATEST_CF_RANGE_PAGE_MAX_ROWS}; remediation=use bounded pages at or below the ceiling"
                ),
            });
        }
        self.with_vault(
            cf_name,
            "scan candidate-bounded ordered Calyx logical page",
            false,
            |vault| read_ordered_rows_from_vault_page(vault, cf_name, start_key, max_rows),
        )
    }

    fn scan_cf_physical_page(
        &self,
        cf_name: &str,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage> {
        calyx_collection_id_for_cf_read(cf_name)?;
        validate_physical_page_request(cf_name, after_physical, max_rows)?;
        if max_rows == 0 {
            return Ok(PhysicalScanPage::empty());
        }
        self.with_vault(
            cf_name,
            "scan candidate-bounded physical Calyx KV namespace page",
            false,
            |vault| read_physical_page_from_vault(vault, cf_name, after_physical, max_rows),
        )
    }

    fn scan_cf_range(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        if fixed_width_user_key_len(cf_name).is_none() {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail:
                    "bounded Calyx range scan is only available for fixed-width key column families"
                        .to_owned(),
            });
        }
        self.with_vault(cf_name, "scan Calyx KV fixed-key range", false, |vault| {
            read_fixed_width_rows_from_vault_range(vault, cf_name, start_key, end_key, max_rows)
        })
    }

    fn scan_cf_fixed_width_range_page(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        after_key: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage> {
        calyx_collection_id_for_cf_read(cf_name)?;
        let key_len = fixed_width_user_key_len(cf_name).unwrap_or(start_key.len());
        validate_fixed_width_page_request(
            cf_name, start_key, end_key, after_key, key_len, max_rows,
        )?;
        if max_rows == 0 {
            return Ok(FixedWidthScanPage::empty());
        }
        self.with_vault(
            cf_name,
            "scan Calyx KV explicit fixed-width range page",
            false,
            |vault| {
                read_fixed_width_page_from_vault_range(
                    vault, cf_name, start_key, end_key, after_key, key_len, max_rows,
                )
            },
        )
    }

    fn pin_cf_physical_scan(
        &self,
        cf_name: &str,
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease> {
        self.vault.pin_cf_physical_scan(cf_name, max_age_ms)
    }

    fn pin_cf_fixed_width_range_scan(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease> {
        self.vault
            .pin_cf_fixed_width_range_scan(cf_name, start_key, end_key, max_age_ms)
    }

    fn scan_cf_physical_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage> {
        self.vault.scan_cf_physical_page_coherent(lease, max_rows)
    }

    fn scan_cf_fixed_width_range_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage> {
        self.vault
            .scan_cf_fixed_width_range_page_coherent(lease, max_rows)
    }

    fn release_coherent_scan(&self, lease: &mut CoherentScanLease) -> StorageResult<bool> {
        self.vault.release_coherent_scan(lease)
    }

    /// The last `max_rows` live rows of one column family, in ascending logical
    /// key order, read with **bounded** row-guard holds.
    ///
    /// `storage inspect` asks for three sample rows per family. The materialising
    /// version answered that by folding the entire family into a vector under one
    /// row-table read guard and then discarding all but the last three — on the
    /// live vault, up to 200,000 rows read and thrown away per family, 17 times,
    /// under the lock every vault write must take exclusively (#2041).
    ///
    /// The sweep keeps a ring of exactly `max_rows` rows instead. The result is
    /// byte-identical because the sweep visits the same live rows in the same
    /// strictly-increasing logical key order the unpaged reader asserted.
    fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>> {
        if max_rows == 0 {
            return Ok(Vec::new());
        }
        let mut tail: std::collections::VecDeque<RawRow> =
            std::collections::VecDeque::with_capacity(max_rows);
        self.with_vault(
            cf_name,
            "read the tail of a Calyx namespace with bounded row-guard holds",
            false,
            |vault| {
                sweep_calyx_namespace_live_rows(vault, cf_name, "scan_cf_tail", |key, payload| {
                    if tail.len() == max_rows {
                        tail.pop_front();
                    }
                    tail.push_back((key.to_vec(), payload.to_vec()));
                    Ok(())
                })
                .map(|_sweep| ())
            },
        )?;
        Ok(tail.into())
    }

    fn compact_cf(&self, cf_name: &str) -> StorageResult<()> {
        calyx_collection_id_for_cf_write(cf_name)?;
        self.with_vault(
            cf_name,
            "compact logical Calyx storage namespace",
            true,
            |vault| {
                vault.purge_kv_tombstones().map_err(|source| {
                    calyx_write_failed(
                        cf_name,
                        "compact physical Calyx KV CF and purge tombstones",
                        &source,
                    )
                })?;
                tracing::info!(
                    code = "STORAGE_CALYX_LOGICAL_CF_COMPACTED",
                    cf = cf_name,
                    physical_cf = ColumnFamily::Kv.name(),
                    "compacted the physical Calyx KV CF backing the requested logical namespace"
                );
                Ok(())
            },
        )
    }

    fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()> {
        calyx_collection_id_for_cf_write(cf_name)?;
        if start >= end {
            return Err(calyx_write_failed_detail(
                cf_name,
                format!(
                    "invalid Calyx compaction range: start must be strictly below end; start_len={} end_len={}",
                    start.len(),
                    end.len()
                ),
            ));
        }
        self.with_vault(
            cf_name,
            "compact logical Calyx storage range",
            true,
            |vault| {
                vault.purge_kv_tombstones().map_err(|source| {
                    calyx_write_failed(
                        cf_name,
                        "compact physical Calyx KV CF and purge range tombstones",
                        &source,
                    )
                })?;
                tracing::info!(
                    code = "STORAGE_CALYX_LOGICAL_CF_RANGE_COMPACTED",
                    cf = cf_name,
                    start_len = start.len(),
                    end_len = end.len(),
                    physical_cf = ColumnFamily::Kv.name(),
                    "compacted the physical Calyx KV CF that durably contains the requested logical range"
                );
                Ok(())
            },
        )
    }
}

/// Scans one storage column family without opening the backend for writes.
///
/// # Errors
///
/// Returns a storage error when the backend cannot be opened read-only, the
/// requested column family is not part of the Synapse schema, the schema
/// sentinel is missing or mismatched, or the rows cannot be read.
pub fn scan_cf_read_only(
    path: &Path,
    schema_version: u32,
    backend: StorageBackendKind,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    scan_cf_read_only_with_expired(path, schema_version, backend, cf_name, false)
}

/// Scans one storage column family without opening the backend for writes,
/// optionally including expired Calyx rows retained in the physical vault.
///
/// # Errors
///
/// Returns a storage error when the backend cannot be opened read-only, the
/// requested column family is not part of the Synapse schema, the schema
/// sentinel is missing or mismatched, or the rows cannot be read.
pub fn scan_cf_read_only_with_expired(
    path: &Path,
    schema_version: u32,
    backend: StorageBackendKind,
    cf_name: &str,
    include_expired: bool,
) -> StorageResult<Vec<RawRow>> {
    match backend {
        StorageBackendKind::Calyx if include_expired => {
            scan_calyx_cf_read_only_including_expired(path, schema_version, cf_name)
        }
        StorageBackendKind::Calyx => scan_calyx_cf_read_only(path, schema_version, cf_name),
    }
}

/// Builds a metadata-only dump of one storage column family without opening it for writes.
///
/// # Errors
///
/// Returns a storage error when the backend cannot be opened read-only, the
/// requested column family is not part of the Synapse schema, the schema
/// sentinel is missing or mismatched, or the rows cannot be read.
pub fn dump_cf_read_only(
    path: &Path,
    schema_version: u32,
    backend: StorageBackendKind,
    cf_name: &str,
) -> StorageResult<StorageCfDump> {
    dump_cf_read_only_with_expired(path, schema_version, backend, cf_name, false)
}

/// Builds a metadata-only dump of one storage column family without opening it
/// for writes, optionally including expired Calyx rows retained in the physical vault.
///
/// # Errors
///
/// Returns a storage error when the backend cannot be opened read-only, the
/// requested column family is not part of the Synapse schema, the schema
/// sentinel is missing or mismatched, or the rows cannot be read.
pub fn dump_cf_read_only_with_expired(
    path: &Path,
    schema_version: u32,
    backend: StorageBackendKind,
    cf_name: &str,
    include_expired: bool,
) -> StorageResult<StorageCfDump> {
    let rows =
        scan_cf_read_only_with_expired(path, schema_version, backend, cf_name, include_expired)?;
    let row_count = u64::try_from(rows.len()).map_err(|_error| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail: format!("storage dump row count does not fit in u64: {}", rows.len()),
    })?;
    Ok(StorageCfDump {
        backend,
        cf_name: cf_name.to_owned(),
        row_count,
        rows: rows
            .iter()
            .map(|(key, value)| storage_dump_row(key, value))
            .collect(),
    })
}

/// Inspects the physical Calyx vault collections without opening a writer.
///
/// # Errors
///
/// Returns a storage error when the Calyx vault is missing, cannot be opened
/// read-only, has a mismatched schema sentinel, or contains malformed Synapse
/// value envelopes.
pub fn inspect_calyx_vault_read_only(
    path: &Path,
    schema_version: u32,
) -> StorageResult<CalyxVaultInspect> {
    let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
    let vault = SynapseCalyxReadOnlyVault::open_existing_kv_only(config)
        .map_err(|source| calyx_open_failed(path, &source))?;
    let actual = verify_calyx_schema_version_existing(&vault, path, schema_version)?;
    require_calyx_ordered_key_migration(&vault, path)?;
    inspect_calyx_vault_with_schema(&vault, path, actual)
}

fn scan_calyx_cf_read_only(
    path: &Path,
    schema_version: u32,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    require_known_cf_for_read(cf_name)?;
    let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
    let vault = SynapseCalyxReadOnlyVault::open_existing_kv_only(config)
        .map_err(|source| calyx_open_failed(path, &source))?;
    verify_calyx_schema_version_existing(&vault, path, schema_version)?;
    require_calyx_ordered_key_migration(&vault, path)?;
    read_all_rows_from_vault(&vault, cf_name)
}

pub fn scan_calyx_cf_read_only_including_expired(
    path: &Path,
    schema_version: u32,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    require_known_cf_for_read(cf_name)?;
    let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
    let vault = SynapseCalyxReadOnlyVault::open_existing_kv_only(config)
        .map_err(|source| calyx_open_failed(path, &source))?;
    verify_calyx_schema_version_existing(&vault, path, schema_version)?;
    require_calyx_ordered_key_migration(&vault, path)?;
    read_all_rows_from_vault_including_expired(&vault, cf_name)
}

fn require_known_cf_for_read(cf_name: &str) -> StorageResult<&'static str> {
    cf::ALL_COLUMN_FAMILIES
        .iter()
        .copied()
        .find(|known| *known == cf_name)
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "column family name is not part of the Synapse storage schema".to_owned(),
        })
}

fn storage_dump_row(key: &[u8], value: &[u8]) -> StorageDumpRow {
    StorageDumpRow {
        key_len_bytes: key.len() as u64,
        key_sha256: sha256_hex(key),
        key_material_omitted: true,
        value_len_bytes: value.len() as u64,
        value_sha256: sha256_hex(value),
        value_encoding: classify_value_encoding(value),
        value_content_omitted: true,
        redaction_policy: STORAGE_METADATA_ONLY_REDACTION_POLICY.to_owned(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(64 + "sha256:".len());
    encoded.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn classify_value_encoding(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "empty".to_owned();
    }
    if std::str::from_utf8(bytes).is_err() {
        return "binary_or_invalid_utf8".to_owned();
    }
    if serde_json::from_slice::<serde_json::Value>(bytes).is_ok() {
        return "json".to_owned();
    }
    "utf8_non_json".to_owned()
}

trait CalyxVaultKvRead {
    fn vault_id_string(&self) -> String;
    fn latest_seq_value(&self) -> u64;
    fn snapshot_gc_state(&self) -> SynapseCalyxSnapshotGcObservation;
    fn clock_now_ms(&self) -> Result<u64, SynapseCalyxError>;
    fn read_kv_latest(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SynapseCalyxError>;
    fn scan_kv_range_latest(
        &self,
        range: &calyx_aster::cf::KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError>;
    fn scan_kv_range_page_latest(
        &self,
        range: &calyx_aster::cf::KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError>;
    fn walk_kv_range_latest_snapshot<V>(
        &self,
        range: &calyx_aster::cf::KeyRange,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>;
}

impl CalyxVaultKvRead for SynapseCalyxVault {
    fn vault_id_string(&self) -> String {
        self.vault_id()
    }

    fn latest_seq_value(&self) -> u64 {
        self.latest_seq()
    }

    fn snapshot_gc_state(&self) -> SynapseCalyxSnapshotGcObservation {
        self.snapshot_gc_observation()
    }

    fn clock_now_ms(&self) -> Result<u64, SynapseCalyxError> {
        self.clock_now_ms()
    }

    fn read_kv_latest(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.read_cf_latest(ColumnFamily::Kv, key)
    }

    fn scan_kv_range_latest(
        &self,
        range: &calyx_aster::cf::KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_range_latest(ColumnFamily::Kv, range)
    }

    fn scan_kv_range_page_latest(
        &self,
        range: &calyx_aster::cf::KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError> {
        self.scan_cf_range_page_latest(ColumnFamily::Kv, range, after_key, limit)
    }

    fn walk_kv_range_latest_snapshot<V>(
        &self,
        range: &calyx_aster::cf::KeyRange,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        self.walk_cf_range_latest_snapshot(ColumnFamily::Kv, range, page_rows, visit)
    }
}

impl CalyxVaultKvRead for SynapseCalyxReadOnlyVault {
    fn vault_id_string(&self) -> String {
        self.vault_id()
    }

    fn latest_seq_value(&self) -> u64 {
        self.latest_seq()
    }

    fn snapshot_gc_state(&self) -> SynapseCalyxSnapshotGcObservation {
        self.snapshot_gc_observation()
    }

    fn clock_now_ms(&self) -> Result<u64, SynapseCalyxError> {
        self.clock_now_ms()
    }

    fn read_kv_latest(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.read_cf_latest(ColumnFamily::Kv, key)
    }

    fn scan_kv_range_latest(
        &self,
        range: &calyx_aster::cf::KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_range_latest(ColumnFamily::Kv, range)
    }

    fn scan_kv_range_page_latest(
        &self,
        range: &calyx_aster::cf::KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<SynapseCalyxCfRangePage, SynapseCalyxError> {
        self.scan_cf_range_page_latest(ColumnFamily::Kv, range, after_key, limit)
    }

    fn walk_kv_range_latest_snapshot<V>(
        &self,
        range: &calyx_aster::cf::KeyRange,
        page_rows: usize,
        visit: V,
    ) -> Result<SynapseCalyxCfWalk, SynapseCalyxError>
    where
        V: FnMut(&[u8], &[u8]) -> Result<SynapseCalyxWalkStep, SynapseCalyxError>,
    {
        self.walk_cf_range_latest_snapshot(ColumnFamily::Kv, range, page_rows, visit)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive source-CF dispatch keeps the declared backfill population and its authoritative decoder/builder mapping reviewable in one place"
)]
fn build_temporal_backfill_row(
    vault: &SynapseCalyxVault,
    build: TemporalBackfillBuildContext<'_>,
    key: &[u8],
    raw: &[u8],
) -> StorageResult<PreparedTemporalBackfillRow> {
    let identity_input = if build.source_cf == SYN_MCP_USAGE_BACKFILL_SOURCE {
        constellations::mcp_usage_constellation_input_bytes(cf::CF_KV, key, raw)
    } else if build.source_cf == SYN_OUTCOME_BACKFILL_SOURCE {
        constellations::outcome_constellation_input_bytes(cf::CF_KV, key, raw)
    } else {
        raw.to_vec()
    };
    let context = NativeConstellationContext {
        vault_id: build.vault_id,
        cx_id: vault.cx_id_for_input(&identity_input, build.active_panel_version),
        created_at_ms: build.created_at_ms,
        next_ledger_seq: build.next_ledger_seq,
    };
    let decode_failed = |error: &serde_json::Error, label: &str| StorageError::ReadFailed {
        cf_name: build.source_cf.to_owned(),
        detail: format!(
            "decode authoritative {label} row key_hex={}: {error}",
            constellations::hex_encode(key)
        ),
    };
    let mut transcript_outcome = None;
    let mut action_outcome_present = None;
    let (constellation, measurement_plan) = constellations::capture_deferred_measurements(|| {
        let constellation = match build.source_cf {
            cf::CF_TIMELINE => {
                let record: TimelineRecord = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "timeline"))?;
                constellations::build_timeline_constellation(context, key, raw, &record)?
            }
            cf::CF_EPISODES => {
                let record: EpisodeRecord = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "episode"))?;
                constellations::build_episode_constellation(context, key, raw, &record)?
            }
            cf::CF_AGENT_TRANSCRIPTS => {
                let record: AgentTranscriptRecord = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "agent transcript"))?;
                transcript_outcome = Some(PreparedTranscriptBackfillOutcome {
                    anchor: constellations::agent_transcript_outcome_anchor(&record),
                    unadjudicable: matches!(
                        constellations::agent_transcript_tool_outcome(&record),
                        constellations::AgentTranscriptToolOutcome::Unadjudicable(_)
                    ),
                });
                constellations::build_agent_transcript_constellation(context, key, raw, &record)?
            }
            cf::CF_AGENT_EVENTS => {
                let record: AgentEventRecord = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "agent event"))?;
                constellations::build_agent_event_constellation(context, key, raw, &record)?
            }
            cf::CF_ACTION_LOG => {
                let record: Value =
                    serde_json::from_slice(raw).map_err(|error| decode_failed(&error, "action"))?;
                action_outcome_present =
                    Some(constellations::action_outcome_anchor(key, &record)?.is_some());
                constellations::build_action_constellation(context, key, raw, &record)?
            }
            cf::CF_REFLEX_AUDIT => {
                let record: StoredReflexAudit = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "reflex audit"))?;
                constellations::build_reflex_audit_constellation(context, key, raw, &record)?
            }
            cf::CF_OBSERVATIONS => {
                let record: StoredObservation = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "observation"))?;
                constellations::build_observation_constellation(context, key, raw, &record)?
            }
            SYN_MCP_USAGE_BACKFILL_SOURCE => {
                let record: Value = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "MCP usage"))?;
                constellations::build_mcp_usage_constellation(
                    context,
                    key,
                    raw,
                    &identity_input,
                    &record,
                )?
            }
            SYN_OUTCOME_BACKFILL_SOURCE => {
                let record: Value = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "outcome"))?;
                constellations::build_outcome_constellation(
                    context,
                    cf::CF_KV,
                    key,
                    raw,
                    &identity_input,
                    &record,
                )?
            }
            // Exhaustive over `backfill_temporal_metadata`'s source guard. A CF
            // added there without a builder here fails through the process decoder
            // instead of silently emitting an empty constellation.
            _ => {
                let record: Value = serde_json::from_slice(raw)
                    .map_err(|error| decode_failed(&error, "process"))?;
                constellations::build_process_constellation(context, key, raw, &record)?
            }
        };
        Ok(constellation)
    })?;
    Ok(PreparedTemporalBackfillRow {
        constellation,
        measurement_plan,
        transcript_outcome,
        action_outcome_present,
    })
}

fn inspect_calyx_vault(
    vault: &impl CalyxVaultKvRead,
    path: &Path,
) -> StorageResult<CalyxVaultInspect> {
    let schema_version = verify_calyx_schema_version_existing(vault, path, 0)?;
    inspect_calyx_vault_with_schema(vault, path, schema_version)
}

fn inspect_calyx_vault_with_schema(
    vault: &impl CalyxVaultKvRead,
    _path: &Path,
    schema_version: u32,
) -> StorageResult<CalyxVaultInspect> {
    let inspected_at_unix_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed("<calyx-vault>", "read Calyx vault clock", &source))?;
    let mut census = CalyxVaultCensus::default();
    // Streamed rather than materialised (#2041/#2243). The whole-vault census
    // used to be one wide guarded scan, then a page loop that reopened every
    // immutable file thousands of times. One registered snapshot plus one
    // sequential cursor now keeps the answer exact without either transient.
    let sweep = sweep_kv_range_pages(
        vault,
        "<calyx-vault>",
        "calyx_vault_inspect",
        &prefix_range(&[CALYX_KV_DISC]),
        |full_key, stored_value| {
            census.add_row(full_key, stored_value, inspected_at_unix_ms)?;
            Ok(ControlFlow::Continue(()))
        },
    )?;
    let CalyxVaultCensus {
        collections,
        totals,
    } = census;
    // One `info` record per whole-vault census, because the per-page sweep record
    // above is `debug` and the daemon runs at `info`: without this the single
    // largest read in `storage inspect` would leave no attributable trace at the
    // level the daemon actually logs at, which is the state #2041 had to
    // reconstruct from `CALYX_ASTER_ROW_READ_GUARD_SLOW` counts.
    tracing::info!(
        code = "STORAGE_CALYX_VAULT_CENSUS_DONE",
        site = "calyx_vault_inspect",
        pages = sweep.pages,
        page_rows = CALYX_INSPECT_SWEEP_PAGE_ROWS,
        rows_visited = sweep.rows_visited,
        rows_examined = sweep.rows_examined,
        collections = collections.len(),
        snapshot_seq_first = sweep.snapshot_seq_first,
        snapshot_seq_last = sweep.snapshot_seq_last,
        atomic = sweep.atomic(),
        "counted every physical Calyx KV collection with bounded row-table read-guard holds"
    );
    Ok(CalyxVaultInspect {
        schema_version,
        vault_id: vault.vault_id_string(),
        latest_seq: vault.latest_seq_value(),
        snapshot_gc: vault.snapshot_gc_state(),
        inspected_at_unix_ms,
        collection_count: collections.len() as u64,
        raw_row_count: totals.raw_row_count,
        live_row_count: totals.live_row_count,
        expired_row_count: totals.expired_row_count,
        user_key_bytes: totals.user_key_bytes,
        payload_bytes: totals.payload_bytes,
        stored_value_bytes: totals.stored_value_bytes,
        total_logical_bytes: totals.total_logical_bytes,
        census_pages: sweep.pages as u64,
        census_page_rows: CALYX_INSPECT_SWEEP_PAGE_ROWS as u64,
        census_snapshot_seq_first: sweep.snapshot_seq_first,
        census_snapshot_seq_last: sweep.snapshot_seq_last,
        census_atomic: sweep.atomic(),
        collections,
    })
}

/// The whole-vault inspection fold, accumulated one page at a time (#2041).
///
/// Extracted from `inspect_calyx_vault_with_schema` so the per-row accounting is
/// a named operation rather than a closure body: the sweep hands it rows in
/// bounded batches now, and the accounting is identical whether it arrives as
/// one materialised vector or as 4,008 pages of 256.
#[derive(Default)]
struct CalyxVaultCensus {
    collections: BTreeMap<String, CalyxVaultCollectionInspect>,
    totals: CalyxVaultTotals,
}

impl CalyxVaultCensus {
    fn add_row(
        &mut self,
        full_key: &[u8],
        stored_value: &[u8],
        inspected_at_unix_ms: u64,
    ) -> StorageResult<()> {
        let key = decode_calyx_key_parts(full_key).map_err(|detail| StorageError::ReadFailed {
            cf_name: "<calyx-vault>".to_owned(),
            detail,
        })?;
        let collection_name = calyx_collection_report_name(key.collection_id, key.namespace);
        let entry = self
            .collections
            .entry(collection_name.clone())
            .or_insert_with(|| CalyxVaultCollectionInspect {
                collection_name,
                cf_name: cf_name_for_calyx_collection_id(key.collection_id).map(str::to_owned),
                collection_id_hex: format!("0x{:016x}", key.collection_id),
                namespace: key.namespace,
                raw_row_count: 0,
                live_row_count: 0,
                expired_row_count: 0,
                user_key_bytes: 0,
                payload_bytes: 0,
                stored_value_bytes: 0,
                total_logical_bytes: 0,
                expires_at_ms_histogram: BTreeMap::new(),
            });
        let envelope =
            decode_calyx_value_raw(stored_value).map_err(|detail| StorageError::ReadFailed {
                cf_name: entry
                    .cf_name
                    .clone()
                    .unwrap_or_else(|| "<calyx-vault>".to_owned()),
                detail: format!("decode Calyx vault inspection value: {detail}"),
            })?;
        let expired = calyx_value_is_expired(envelope.expires_at_ms, inspected_at_unix_ms);
        let user_key_bytes = key.user_key.len() as u64;
        let payload_bytes = envelope.payload.len() as u64;
        let stored_value_bytes = stored_value.len() as u64;
        let total_logical_bytes =
            user_key_bytes
                .checked_add(payload_bytes)
                .ok_or_else(|| StorageError::ReadFailed {
                    cf_name: entry
                        .cf_name
                        .clone()
                        .unwrap_or_else(|| "<calyx-vault>".to_owned()),
                    detail: "Calyx vault inspection byte accounting overflow".to_owned(),
                })?;
        entry.raw_row_count = entry.raw_row_count.saturating_add(1);
        entry.user_key_bytes = entry.user_key_bytes.saturating_add(user_key_bytes);
        entry.payload_bytes = entry.payload_bytes.saturating_add(payload_bytes);
        entry.stored_value_bytes = entry.stored_value_bytes.saturating_add(stored_value_bytes);
        entry.total_logical_bytes = entry
            .total_logical_bytes
            .saturating_add(total_logical_bytes);
        if expired {
            entry.expired_row_count = entry.expired_row_count.saturating_add(1);
        } else {
            entry.live_row_count = entry.live_row_count.saturating_add(1);
        }
        let bucket = expires_at_histogram_bucket(envelope.expires_at_ms, inspected_at_unix_ms);
        *entry
            .expires_at_ms_histogram
            .entry(bucket.to_owned())
            .or_insert(0) += 1;
        self.totals
            .add(expired, user_key_bytes, payload_bytes, stored_value_bytes)
    }
}

#[derive(Default)]
struct CalyxVaultTotals {
    raw_row_count: u64,
    live_row_count: u64,
    expired_row_count: u64,
    user_key_bytes: u64,
    payload_bytes: u64,
    stored_value_bytes: u64,
    total_logical_bytes: u64,
}

impl CalyxVaultTotals {
    fn add(
        &mut self,
        expired: bool,
        user_key_bytes: u64,
        payload_bytes: u64,
        stored_value_bytes: u64,
    ) -> StorageResult<()> {
        self.raw_row_count = self.raw_row_count.saturating_add(1);
        if expired {
            self.expired_row_count = self.expired_row_count.saturating_add(1);
        } else {
            self.live_row_count = self.live_row_count.saturating_add(1);
        }
        self.user_key_bytes = self.user_key_bytes.saturating_add(user_key_bytes);
        self.payload_bytes = self.payload_bytes.saturating_add(payload_bytes);
        self.stored_value_bytes = self.stored_value_bytes.saturating_add(stored_value_bytes);
        self.total_logical_bytes = self
            .total_logical_bytes
            .checked_add(user_key_bytes.checked_add(payload_bytes).ok_or_else(|| {
                StorageError::ReadFailed {
                    cf_name: "<calyx-vault>".to_owned(),
                    detail: "Calyx vault total byte accounting overflow".to_owned(),
                }
            })?)
            .ok_or_else(|| StorageError::ReadFailed {
                cf_name: "<calyx-vault>".to_owned(),
                detail: "Calyx vault total byte accounting overflow".to_owned(),
            })?;
        Ok(())
    }
}

struct CalyxKeyParts {
    collection_id: u64,
    namespace: u64,
    user_key: Vec<u8>,
}

fn decode_calyx_key_parts(full_key: &[u8]) -> Result<CalyxKeyParts, String> {
    if full_key.len() < 1 + 8 + 8 {
        return Err(format!(
            "Calyx KV key is shorter than the Synapse envelope header: len={}",
            full_key.len()
        ));
    }
    if full_key[0] != CALYX_KV_DISC {
        return Err(format!(
            "Calyx KV key has unsupported discriminator 0x{:02x}; expected 0x{CALYX_KV_DISC:02x}",
            full_key[0]
        ));
    }
    let mut collection_bytes = [0_u8; 8];
    collection_bytes.copy_from_slice(&full_key[1..9]);
    let mut namespace_bytes = [0_u8; 8];
    namespace_bytes.copy_from_slice(&full_key[9..17]);
    let collection_id = u64::from_be_bytes(collection_bytes);
    let namespace = u64::from_be_bytes(namespace_bytes);
    let user_key = match namespace {
        CALYX_KV_LEGACY_LENGTH_ORDERED_NAMESPACE => {
            decode_calyx_legacy_user_key(collection_id, full_key)?
        }
        CALYX_KV_ORDERED_NAMESPACE => decode_calyx_user_key(collection_id, full_key)?,
        other => {
            return Err(format!(
                "Calyx KV key has unsupported namespace {other}; supported legacy={CALYX_KV_LEGACY_LENGTH_ORDERED_NAMESPACE} ordered={CALYX_KV_ORDERED_NAMESPACE}"
            ));
        }
    };
    Ok(CalyxKeyParts {
        collection_id,
        namespace,
        user_key,
    })
}

const fn expires_at_histogram_bucket(expires_at_ms: u64, now_ms: u64) -> &'static str {
    if expires_at_ms == 0 {
        return "durable";
    }
    if now_ms >= expires_at_ms {
        return "expired";
    }
    let remaining = expires_at_ms - now_ms;
    if remaining <= MILLIS_PER_HOUR {
        "expires_within_1h"
    } else if remaining <= MILLIS_PER_DAY {
        "expires_within_24h"
    } else if remaining <= 7 * MILLIS_PER_DAY {
        "expires_within_7d"
    } else {
        "expires_after_7d"
    }
}

fn calyx_collection_report_name(collection_id: u64, namespace: u64) -> String {
    if collection_id == CALYX_METADATA_COLLECTION_ID {
        return format!("__synapse_metadata/ns/{namespace}");
    }
    cf_name_for_calyx_collection_id(collection_id).map_or_else(
        || format!("unknown/0x{collection_id:016x}/ns/{namespace}"),
        |cf_name| format!("{cf_name}/ns/{namespace}"),
    )
}

fn cf_name_for_calyx_collection_id(collection_id: u64) -> Option<&'static str> {
    for (offset, known_cf) in (1_u64..).zip(cf::ALL_COLUMN_FAMILIES) {
        if collection_id == (CALYX_COLLECTION_ID_BASE | offset) {
            return Some(known_cf);
        }
    }
    None
}

struct ConstellationReportInput<'a> {
    panel_name: &'static str,
    panel_version: u32,
    source_cf: &'static str,
    source_key: &'a [u8],
    raw_bytes: &'a [u8],
    readback: SynapseCalyxObservationPutReadback,
    slot_count: u64,
    scalar_count: u64,
    duration_us: u64,
}

struct PreparedReflexRegistrationPublication {
    context: NativeConstellationContext,
    physical_guards: Vec<SynapseCalyxRevisionGuard>,
    physical_rows: Vec<SynapseCalyxCfWrite>,
    constellation: Constellation,
    slot_count: u64,
    scalar_count: u64,
    anchor: Anchor,
    ledger_payload: Vec<u8>,
}

fn validate_reflex_registration_publication(
    guards: &[CfRevisionGuard],
    batches: &[OwnedCfWriteBatch],
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &StoredReflexAudit,
) -> StorageResult<()> {
    const OPERATION_CF: &str = "calyx_reflex_lifecycle_publication";
    validate_cross_cf_revision_guarded_put(guards, batches)?;
    let requested_rows = batches
        .iter()
        .filter(|(cf_name, _rows)| cf_name == cf::CF_REFLEX_AUDIT)
        .flat_map(|(_cf_name, rows)| rows.iter())
        .filter(|(key, _value)| key.as_slice() == source_key)
        .collect::<Vec<_>>();
    if requested_rows.len() != 1 || requested_rows[0].1.as_slice() != raw_bytes {
        return Err(calyx_write_failed_detail(
            OPERATION_CF,
            format!(
                "REFLEX_LIFECYCLE_SOURCE_ROW_INVALID: expected one exact CF_REFLEX_AUDIT source row; matching_rows={} expected_sha256={} actual_sha256={}; remediation=rebuild the complete lifecycle publication from the encoded audit",
                requested_rows.len(),
                constellations::sha256_hex(raw_bytes),
                requested_rows.first().map_or_else(
                    || "absent".to_owned(),
                    |row| constellations::sha256_hex(&row.1),
                )
            ),
        ));
    }
    let kind = record.details.get("kind").and_then(Value::as_str);
    if !matches!(
        (record.status, kind),
        (
            synapse_core::ReflexState::Active,
            Some("reflex_registered" | "reflex_terminal_lifecycle_intent_prepared")
        ) | (
            synapse_core::ReflexState::Cancelled,
            Some("reflex_cancelled")
        ) | (
            synapse_core::ReflexState::Disabled,
            Some("reflex_disabled_by_operator")
        ) | (
            synapse_core::ReflexState::Expired | synapse_core::ReflexState::ActionDenied,
            Some(_)
        )
    ) {
        return Err(calyx_write_failed_detail(
            OPERATION_CF,
            format!(
                "REFLEX_LIFECYCLE_AUDIT_KIND_INVALID: reflex_id={} status={:?} kind={:?}; remediation=route only a supported registration/terminal-intent/cancellation/disable audit through the lifecycle transaction",
                record.reflex_id,
                record.status,
                record.details.get("kind")
            ),
        ));
    }
    Ok(())
}

fn reflex_lifecycle_state(record: &StoredReflexAudit) -> StorageResult<&'static str> {
    match record.status {
        synapse_core::ReflexState::Active => Ok("active"),
        synapse_core::ReflexState::Cancelled => Ok("cancelled"),
        synapse_core::ReflexState::Disabled => Ok("disabled"),
        synapse_core::ReflexState::Expired => Ok("expired"),
        synapse_core::ReflexState::ActionDenied => Ok("action_denied"),
        _ => Err(calyx_write_failed_detail(
            "calyx_reflex_lifecycle_publication",
            format!(
                "REFLEX_LIFECYCLE_ANCHOR_STATE_INVALID: reflex_id={} status={:?}; remediation=validate the lifecycle audit before constellation preparation",
                record.reflex_id, record.status
            ),
        )),
    }
}

fn prepare_reflex_registration_publication(
    vault: &SynapseCalyxVault,
    guards: Vec<CfRevisionGuard>,
    batches: Vec<OwnedCfWriteBatch>,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &StoredReflexAudit,
) -> StorageResult<PreparedReflexRegistrationPublication> {
    const OPERATION_CF: &str = "calyx_reflex_lifecycle_publication";
    let now_ms = calyx_clock_now_for_write(vault, OPERATION_CF)?;
    let mut physical_guards = Vec::with_capacity(guards.len());
    for guard in guards {
        let collection_id = calyx_collection_id_for_cf_write(&guard.cf_name)?;
        let physical_key = encode_calyx_key_for_write(&guard.cf_name, collection_id, &guard.key)?;
        physical_guards.push(SynapseCalyxRevisionGuard::new(
            ColumnFamily::Kv,
            physical_key,
            guard.expected_revision_sha256,
        ));
    }
    let mut physical_rows = Vec::new();
    for (cf_name, rows) in batches {
        let collection_id = calyx_collection_id_for_cf_write(&cf_name)?;
        for (key, value) in rows {
            physical_rows.push(calyx_put_row(
                &cf_name,
                collection_id,
                &key,
                &value,
                now_ms,
            )?);
        }
    }
    let context = NativeConstellationContext {
        vault_id: vault.vault_id_value(),
        cx_id: vault.cx_id_for_input(raw_bytes, SYN_REFLEX_PANEL_VERSION),
        created_at_ms: now_ms,
        next_ledger_seq: vault.latest_seq().saturating_add(1),
    };
    let constellation =
        constellations::build_reflex_audit_constellation(context, source_key, raw_bytes, record)?;
    let slot_count = u64::try_from(constellation.slots.len()).unwrap_or(u64::MAX);
    let scalar_count = u64::try_from(constellation.scalars.len()).unwrap_or(u64::MAX);
    let lifecycle_state = reflex_lifecycle_state(record)?;
    let anchor = grounding_anchor_to_calyx(GroundingAnchor {
        kind_label: "reflex_lifecycle_state".to_owned(),
        value: GroundingAnchorValue::Enum(lifecycle_state.to_owned()),
        source: "synapse-reflex-lifecycle".to_owned(),
        observed_at_ms: record.ts_ns / 1_000_000,
        confidence: 1.0,
    })?;
    let ledger_payload = serde_json::to_vec(&serde_json::json!({
        "schema": "synapse.reflex.lifecycle.v1",
        "state": lifecycle_state,
        "reflex_id": record.reflex_id,
        "audit_id": record.audit_id,
        "audit_ts_ns": record.ts_ns,
        "source_cf": cf::CF_REFLEX_AUDIT,
        "source_key_sha256": constellations::sha256_hex(source_key),
        "source_value_sha256": constellations::sha256_hex(raw_bytes),
        "publication_row_count": physical_rows.len(),
    }))
    .map_err(|source| StorageError::EncodeJson {
        type_name: "reflex_registration_grounding_ledger_payload",
        source,
    })?;
    Ok(PreparedReflexRegistrationPublication {
        context,
        physical_guards,
        physical_rows,
        constellation,
        slot_count,
        scalar_count,
        anchor,
        ledger_payload,
    })
}

fn commit_reflex_registration_publication(
    vault: &SynapseCalyxVault,
    prepared: PreparedReflexRegistrationPublication,
    source_key: &[u8],
    raw_bytes: &[u8],
    started: Instant,
) -> StorageResult<ReflexRegistrationPublicationReport> {
    const OPERATION_CF: &str = "calyx_reflex_lifecycle_publication";
    let expected_rows = prepared.physical_rows.clone();
    let write = vault
        .put_guarded_grounded_observation_with_source_rows(
            prepared.physical_rows,
            prepared.physical_guards,
            raw_bytes.to_vec(),
            prepared.constellation,
            prepared.anchor.clone(),
            prepared.ledger_payload,
            "synapse-reflex-lifecycle",
        )
        .map_err(|source| {
            calyx_write_failed(
                OPERATION_CF,
                "commit atomic reflex lifecycle source/constellation/anchor rows",
                &source,
            )
        })?;
    if write.source_row_count != expected_rows.len() {
        return Err(calyx_write_failed_detail(
            OPERATION_CF,
            format!(
                "REFLEX_LIFECYCLE_COMMITTED_ROW_COUNT_MISMATCH: committed={} expected={}; remediation=preserve the vault and inspect the atomic WAL sequence {}",
                write.source_row_count,
                expected_rows.len(),
                write.committed_seq
            ),
        ));
    }
    let source_readback = verify_grounded_source_readback(
        vault,
        &expected_rows,
        cf::CF_REFLEX_AUDIT,
        source_key,
        raw_bytes,
        OPERATION_CF,
        "reflex lifecycle",
    )?;
    let anchor_count = verify_grounded_anchor_readback(
        vault,
        prepared.context.cx_id,
        &prepared.anchor,
        OPERATION_CF,
        "reflex lifecycle",
    )?;
    let constellation = constellation_report(ConstellationReportInput {
        panel_name: SYN_REFLEX_PANEL_NAME,
        panel_version: SYN_REFLEX_PANEL_VERSION,
        source_cf: cf::CF_REFLEX_AUDIT,
        source_key,
        raw_bytes,
        readback: grounded_observation_as_observation_readback(&write),
        slot_count: prepared.slot_count,
        scalar_count: prepared.scalar_count,
        duration_us: constellations::duration_us(started.elapsed()),
    });
    let anchor = anchor_write_report(AnchorWriteReportInput {
        source_cf: cf::CF_REFLEX_AUDIT,
        source_key,
        raw_bytes,
        panel_name: SYN_REFLEX_PANEL_NAME,
        panel_version: SYN_REFLEX_PANEL_VERSION,
        cx_id: prepared.context.cx_id,
        anchor: &prepared.anchor,
        write: grounded_observation_as_anchor_readback(&write),
        readback_anchor_count: anchor_count,
    });
    Ok(ReflexRegistrationPublicationReport {
        source_row_count: u64::try_from(write.source_row_count).unwrap_or(u64::MAX),
        source_readback_exact_match_count: u64::try_from(source_readback.exact_match_count)
            .unwrap_or(u64::MAX),
        source_key_hex: source_readback.logical_key_hex,
        source_value_len_bytes: source_readback.logical_value_len_bytes,
        source_value_sha256: source_readback.logical_value_sha256,
        committed_seq: write.committed_seq,
        constellation,
        anchor,
    })
}

struct PreparedMcpUsageGroundedPublication {
    context: NativeConstellationContext,
    content_addressed_source_identity: Vec<u8>,
    constellation: Constellation,
    slot_count: u64,
    scalar_count: u64,
    anchor: Anchor,
    ledger_payload: Vec<u8>,
    physical_source_rows: Vec<SynapseCalyxCfWrite>,
}

struct VerifiedGroundedSourceReadback {
    exact_match_count: usize,
    logical_key_hex: String,
    logical_value_len_bytes: u64,
    logical_value_sha256: String,
}

fn validate_mcp_usage_source_rows(
    source_rows: &[RawRow],
    source_key: &[u8],
    raw_bytes: &[u8],
) -> StorageResult<()> {
    let exact_source_count = source_rows
        .iter()
        .filter(|(key, value)| key == source_key && value == raw_bytes)
        .count();
    if exact_source_count != 1 {
        return Err(calyx_write_failed_detail(
            cf::CF_KV,
            format!(
                "atomic MCP usage publication requires exactly one source row matching source_key/raw_bytes; found {exact_source_count}"
            ),
        ));
    }
    Ok(())
}

fn prepare_mcp_usage_grounded_publication(
    vault: &SynapseCalyxVault,
    source_rows: Vec<RawRow>,
    source_key: &[u8],
    raw_bytes: &[u8],
    record: &Value,
    anchor: GroundingAnchor,
    ledger_payload: &Value,
) -> StorageResult<PreparedMcpUsageGroundedPublication> {
    let panel = constellations::anchor_panel_for_source_row(cf::CF_KV, source_key)?;
    if panel.panel_name != SYN_MCP_USAGE_PANEL_NAME
        || panel.panel_version != SYN_MCP_USAGE_PANEL_VERSION
    {
        return Err(calyx_write_failed_detail(
            "calyx_mcp_usage_publication",
            format!(
                "atomic MCP usage publication requires an MCP usage source row; key_hex={} maps to {} version {}",
                constellations::hex_encode(source_key),
                panel.panel_name,
                panel.panel_version
            ),
        ));
    }
    let input_bytes = constellations::source_constellation_input_bytes(
        panel.input_mode,
        cf::CF_KV,
        source_key,
        raw_bytes,
    );
    let context = NativeConstellationContext {
        vault_id: vault.vault_id_value(),
        cx_id: vault.cx_id_for_input(&input_bytes, panel.panel_version),
        created_at_ms: calyx_clock_now_for_write(vault, cf::CF_KV)?,
        next_ledger_seq: vault.latest_seq().saturating_add(1),
    };
    let constellation = constellations::build_mcp_usage_constellation(
        context,
        source_key,
        raw_bytes,
        &input_bytes,
        record,
    )?;
    let slot_count = u64::try_from(constellation.slots.len()).unwrap_or(u64::MAX);
    let scalar_count = u64::try_from(constellation.scalars.len()).unwrap_or(u64::MAX);
    let anchor = grounding_anchor_to_calyx(anchor)?;
    let ledger_payload =
        serde_json::to_vec(ledger_payload).map_err(|source| StorageError::EncodeJson {
            type_name: "calyx_mcp_usage_atomic_ledger_payload",
            source,
        })?;
    let collection_id = calyx_collection_id_for_cf_write(cf::CF_KV)?;
    let now_ms = calyx_clock_now_for_write(vault, cf::CF_KV)?;
    let physical_source_rows = source_rows
        .into_iter()
        .map(|(key, value)| calyx_put_row(cf::CF_KV, collection_id, &key, &value, now_ms))
        .collect::<StorageResult<Vec<_>>>()?;
    Ok(PreparedMcpUsageGroundedPublication {
        context,
        content_addressed_source_identity: input_bytes,
        constellation,
        slot_count,
        scalar_count,
        anchor,
        ledger_payload,
        physical_source_rows,
    })
}

fn commit_mcp_usage_grounded_publication(
    vault: &SynapseCalyxVault,
    prepared: PreparedMcpUsageGroundedPublication,
    source_key: &[u8],
    raw_bytes: &[u8],
    started: Instant,
) -> StorageResult<McpUsageGroundedPublicationReport> {
    let PreparedMcpUsageGroundedPublication {
        context,
        content_addressed_source_identity,
        constellation,
        slot_count,
        scalar_count,
        anchor,
        ledger_payload,
        physical_source_rows,
    } = prepared;
    let expected_source_rows = physical_source_rows.clone();
    let atomic_write_started = Instant::now();
    let write = vault
        .put_grounded_observation_with_source_rows(
            physical_source_rows,
            content_addressed_source_identity,
            constellation,
            anchor.clone(),
            ledger_payload,
            "synapse-mcp-usage",
        )
        .map_err(|source| {
            calyx_write_failed(
                "calyx_mcp_usage_publication",
                "commit atomic MCP usage source/constellation/anchor rows",
                &source,
            )
        })?;
    let atomic_write_us = constellations::duration_us(atomic_write_started.elapsed());
    if write.source_row_count != expected_source_rows.len() {
        return Err(calyx_write_failed_detail(
            "calyx_mcp_usage_publication",
            format!(
                "atomic MCP usage committed source row count mismatch: committed={} expected={}",
                write.source_row_count,
                expected_source_rows.len()
            ),
        ));
    }
    let source_readback_started = Instant::now();
    let source_readback = verify_grounded_source_readback(
        vault,
        &expected_source_rows,
        cf::CF_KV,
        source_key,
        raw_bytes,
        "calyx_mcp_usage_publication",
        "MCP usage",
    )?;
    let source_readback_us = constellations::duration_us(source_readback_started.elapsed());
    let anchor_readback_started = Instant::now();
    let readback_anchor_count = verify_grounded_anchor_readback(
        vault,
        context.cx_id,
        &anchor,
        "calyx_mcp_usage_publication",
        "MCP usage",
    )?;
    let anchor_readback_us = constellations::duration_us(anchor_readback_started.elapsed());
    tracing::info!(
        code = "CALYX_MCP_USAGE_PUBLICATION_STAGE_TIMINGS",
        cx_id = %context.cx_id,
        atomic_write_us,
        source_readback_us,
        exact_anchor_readback_us = anchor_readback_us,
        total_us = constellations::duration_us(started.elapsed()),
        "completed MCP usage publication stage timing readback"
    );
    Ok(finish_mcp_usage_grounded_publication(
        context,
        slot_count,
        scalar_count,
        &anchor,
        &write,
        &source_readback,
        readback_anchor_count,
        source_key,
        raw_bytes,
        started,
    ))
}

fn verify_grounded_source_readback(
    vault: &SynapseCalyxVault,
    expected_source_rows: &[SynapseCalyxCfWrite],
    logical_source_cf: &'static str,
    source_key: &[u8],
    raw_bytes: &[u8],
    operation_cf: &'static str,
    operation_label: &'static str,
) -> StorageResult<VerifiedGroundedSourceReadback> {
    let collection_id = calyx_collection_id_for_cf_write(logical_source_cf)?;
    let requested_physical_key =
        encode_calyx_key_for_write(logical_source_cf, collection_id, source_key)?;
    let mut requested_readback = None;
    let reads = expected_source_rows
        .iter()
        .map(|expected| CfRead::new(ColumnFamily::Kv, expected.key.clone()))
        .collect::<Vec<_>>();
    let actual_rows = vault.read_cf_batch_latest(&reads).map_err(|source| {
        calyx_read_failed(
            logical_source_cf,
            "read back atomic grounded source row batch from one latest view",
            &source,
        )
    })?;
    if actual_rows.len() != expected_source_rows.len() {
        return Err(calyx_write_failed_detail(
            operation_cf,
            format!(
                "atomic {operation_label} physical source readback returned {} rows for {} requested keys",
                actual_rows.len(),
                expected_source_rows.len()
            ),
        ));
    }
    for (expected, actual) in expected_source_rows.iter().zip(actual_rows) {
        let Some(actual) = actual else {
            return Err(calyx_write_failed_detail(
                operation_cf,
                format!(
                    "atomic {operation_label} physical source readback missing: key_hex={}",
                    constellations::hex_encode(&expected.key)
                ),
            ));
        };
        if actual != expected.value {
            return Err(calyx_write_failed_detail(
                operation_cf,
                format!(
                    "atomic {operation_label} physical source readback mismatch: key_hex={} expected_sha256={} actual_sha256={}",
                    constellations::hex_encode(&expected.key),
                    constellations::sha256_hex(&expected.value),
                    constellations::sha256_hex(&actual)
                ),
            ));
        }
        if expected.key == requested_physical_key {
            let envelope = decode_calyx_value_raw(&actual).map_err(|detail| {
                calyx_write_failed_detail(
                    operation_cf,
                    format!(
                        "atomic {operation_label} physical source readback has an invalid retention envelope: key_hex={} detail={detail}",
                        constellations::hex_encode(source_key)
                    ),
                )
            })?;
            if envelope.payload != raw_bytes {
                return Err(calyx_write_failed_detail(
                    operation_cf,
                    format!(
                        "atomic {operation_label} logical payload readback mismatch: key_hex={} expected_sha256={} actual_sha256={}",
                        constellations::hex_encode(source_key),
                        constellations::sha256_hex(raw_bytes),
                        constellations::sha256_hex(envelope.payload)
                    ),
                ));
            }
            requested_readback = Some(VerifiedGroundedSourceReadback {
                exact_match_count: expected_source_rows.len(),
                logical_key_hex: constellations::hex_encode(source_key),
                logical_value_len_bytes: u64::try_from(envelope.payload.len()).unwrap_or(u64::MAX),
                logical_value_sha256: sha256_hex(envelope.payload),
            });
        }
    }
    requested_readback.ok_or_else(|| {
        calyx_write_failed_detail(
            operation_cf,
            format!(
                "atomic {operation_label} physical source readback omitted requested key: key_hex={}",
                constellations::hex_encode(source_key)
            ),
        )
    })
}

fn verify_grounded_anchor_readback(
    vault: &SynapseCalyxVault,
    cx_id: CxId,
    expected_anchor: &Anchor,
    operation_cf: &'static str,
    operation_label: &'static str,
) -> StorageResult<usize> {
    let anchor_row = vault
        .read_anchor_exact(cx_id, &expected_anchor.kind)
        .map_err(|source| {
            calyx_read_failed(
                "calyx_anchors",
                "read back exact atomic grounded anchor",
                &source,
            )
        })?
        .ok_or_else(|| {
            calyx_write_failed_detail(
                operation_cf,
                format!(
                    "atomic {operation_label} exact physical anchor row is missing for cx_id={cx_id} kind={}",
                    anchor_kind_label(&expected_anchor.kind)
                ),
            )
        })?;
    if anchor_row.anchor != *expected_anchor {
        return Err(calyx_write_failed_detail(
            operation_cf,
            format!(
                "atomic {operation_label} exact physical anchor readback mismatch for cx_id={cx_id} kind={}",
                anchor_kind_label(&expected_anchor.kind)
            ),
        ));
    }
    Ok(1)
}

#[allow(
    clippy::too_many_arguments,
    reason = "final report construction receives the independently verified commit facts"
)]
fn finish_mcp_usage_grounded_publication(
    context: NativeConstellationContext,
    slot_count: u64,
    scalar_count: u64,
    expected_anchor: &Anchor,
    write: &SynapseCalyxGroundedObservationReadback,
    source_readback: &VerifiedGroundedSourceReadback,
    readback_anchor_count: usize,
    source_key: &[u8],
    raw_bytes: &[u8],
    started: Instant,
) -> McpUsageGroundedPublicationReport {
    let observation = grounded_observation_as_observation_readback(write);
    let anchor_write = grounded_observation_as_anchor_readback(write);
    let duration_us = constellations::duration_us(started.elapsed());
    let constellation = constellation_report(ConstellationReportInput {
        panel_name: SYN_MCP_USAGE_PANEL_NAME,
        panel_version: SYN_MCP_USAGE_PANEL_VERSION,
        source_cf: cf::CF_KV,
        source_key,
        raw_bytes,
        readback: observation,
        slot_count,
        scalar_count,
        duration_us,
    });
    let anchor = anchor_write_report(AnchorWriteReportInput {
        source_cf: cf::CF_KV,
        source_key,
        raw_bytes,
        panel_name: SYN_MCP_USAGE_PANEL_NAME,
        panel_version: SYN_MCP_USAGE_PANEL_VERSION,
        cx_id: context.cx_id,
        anchor: expected_anchor,
        write: anchor_write,
        readback_anchor_count,
    });
    tracing::info!(
        code = "CALYX_MCP_USAGE_ATOMIC_PUBLICATION_COMMITTED",
        source_row_count = write.source_row_count,
        source_readback_exact_match_count = source_readback.exact_match_count,
        source_key_hex = %source_readback.logical_key_hex,
        source_value_len_bytes = source_readback.logical_value_len_bytes,
        source_value_sha256 = %source_readback.logical_value_sha256,
        cx_id = %context.cx_id,
        ledger_seq = write.ledger_seq,
        committed_seq = write.committed_seq,
        latest_seq = write.latest_seq,
        duration_us,
        "MCP usage source rows, constellation, anchor, and ledger entry committed atomically with physical readback"
    );
    McpUsageGroundedPublicationReport {
        source_row_count: u64::try_from(write.source_row_count).unwrap_or(u64::MAX),
        source_readback_exact_match_count: u64::try_from(source_readback.exact_match_count)
            .unwrap_or(u64::MAX),
        source_key_hex: source_readback.logical_key_hex.clone(),
        source_value_len_bytes: source_readback.logical_value_len_bytes,
        source_value_sha256: source_readback.logical_value_sha256.clone(),
        committed_seq: write.committed_seq,
        constellation,
        anchor,
    }
}

fn constellation_report(input: ConstellationReportInput<'_>) -> ConstellationPutReport {
    ConstellationPutReport {
        panel_name: input.panel_name,
        panel_version: input.panel_version,
        source_cf: input.source_cf,
        source_key_hex: constellations::hex_encode(input.source_key),
        raw_sha256: constellations::sha256_hex(input.raw_bytes),
        cx_id: input.readback.cx_id,
        disposition: input.readback.disposition,
        latest_seq: input.readback.latest_seq,
        slot_count: input.slot_count,
        scalar_count: input.scalar_count,
        duration_us: input.duration_us,
    }
}

fn put_recurrence_subject_occurrence_on_vault(
    vault: &SynapseCalyxVault,
    kind: RecurrenceSubjectKind,
    subject_id: &str,
    event_time_ns: u64,
    occurrence_identity: &[u8],
    context: &[u8],
    region_trigger_cx_id: Option<calyx_core::CxId>,
) -> StorageResult<CalyxRecurrenceSubjectReport> {
    let subject_id = validate_recurrence_subject_id(subject_id)?;
    if occurrence_identity.is_empty() {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_recurrence".to_owned(),
            detail: format!(
                "recurrence subject kind={} id={subject_id:?} requires a non-empty caller-stable occurrence identity",
                kind.as_str()
            ),
        });
    }
    let input = constellations::recurrence_subject_input_bytes(kind, &subject_id);
    let cx_id = vault.cx_id_for_input(&input, SYN_RECURRENCE_SUBJECT_PANEL_VERSION);
    let created_at_ms = calyx_clock_now_for_write(vault, "calyx_recurrence")?;
    let constellation = constellations::build_recurrence_subject_constellation(
        NativeConstellationContext {
            vault_id: vault.vault_id_value(),
            cx_id,
            created_at_ms,
            next_ledger_seq: vault.latest_seq().saturating_add(1),
        },
        kind,
        &subject_id,
        &input,
    )?;
    let subject = vault
        .put_observation_constellation(constellation)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_recurrence",
                "put native Calyx recurrence subject constellation",
                &source,
            )
        })?;
    let event_time_secs = i64::try_from(event_time_ns / 1_000_000_000).map_err(|error| {
        StorageError::WriteFailed {
            cf_name: "calyx_recurrence".to_owned(),
            detail: format!(
                "recurrence event time {event_time_ns}ns does not fit EpochSecs for kind={} id={subject_id:?}: {error}",
                kind.as_str()
            ),
        }
    })?;
    let observed_at_secs =
        i64::try_from(created_at_ms / 1_000).map_err(|error| StorageError::WriteFailed {
            cf_name: "calyx_recurrence".to_owned(),
            detail: format!("Calyx clock {created_at_ms}ms does not fit EpochSecs: {error}"),
        })?;
    let mut identity_hasher = Sha256::new();
    identity_hasher.update(b"synapse-recurrence-occurrence-v1");
    identity_hasher.update([0]);
    identity_hasher.update(kind.as_str().as_bytes());
    identity_hasher.update([0]);
    identity_hasher.update(subject_id.as_bytes());
    identity_hasher.update([0]);
    identity_hasher.update(occurrence_identity);
    let occurrence_identity_sha256: [u8; 32] = identity_hasher.finalize().into();
    let occurrence = region_trigger_cx_id
        .map_or_else(
            || {
                vault.append_recurrence_occurrence_once(
                    cx_id,
                    event_time_secs,
                    observed_at_secs,
                    context.to_vec(),
                    occurrence_identity_sha256,
                )
            },
            |trigger_cx_id| {
                vault
                    .append_recurrence_occurrence_once_with_region(
                        cx_id,
                        event_time_secs,
                        observed_at_secs,
                        context.to_vec(),
                        occurrence_identity_sha256,
                        kind.as_str(),
                        &subject_id,
                        trigger_cx_id,
                    )
                    .map(|readback| readback.occurrence)
            },
        )
        .map_err(|source| {
            calyx_write_failed(
                "calyx_recurrence",
                "append native Calyx recurrence subject occurrence",
                &source,
            )
        })?;
    Ok(CalyxRecurrenceSubjectReport {
        subject_kind: kind.as_str().to_owned(),
        subject_id,
        subject_cx_id: cx_id.to_string(),
        subject_panel_name: SYN_RECURRENCE_SUBJECT_PANEL_NAME.to_owned(),
        subject_panel_version: SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
        subject_disposition: subject.disposition,
        occurrence,
    })
}

fn validate_recurrence_subject_id(subject_id: &str) -> StorageResult<String> {
    let trimmed = subject_id.trim();
    if trimmed.is_empty() {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_recurrence".to_owned(),
            detail: "recurrence subject id must not be empty".to_owned(),
        });
    }
    if trimmed.len() > 200 {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_recurrence".to_owned(),
            detail: format!(
                "recurrence subject id is {} bytes; maximum is 200",
                trimmed.len()
            ),
        });
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn grounded_observation_as_observation_readback(
    readback: &SynapseCalyxGroundedObservationReadback,
) -> SynapseCalyxObservationPutReadback {
    SynapseCalyxObservationPutReadback {
        cx_id: readback.cx_id.clone(),
        disposition: readback.disposition,
        latest_seq: readback.latest_seq,
    }
}

fn grounded_observation_as_anchor_readback(
    readback: &SynapseCalyxGroundedObservationReadback,
) -> SynapseCalyxAnchorWriteReadback {
    SynapseCalyxAnchorWriteReadback {
        cx_id: readback.cx_id.clone(),
        anchor_count: 1,
        ledger_seq: readback.ledger_seq,
        ledger_hash: readback.ledger_hash.clone(),
        latest_seq: readback.latest_seq,
    }
}

/// Refuses an outcome-axis measurement over a panel the catalog declares
/// observation-shaped (#1962 ask 3).
///
/// `bits` and `sufficiency` measure *about an anchor*. On an observation-shaped
/// panel there is no anchor and there never will be one — a timeline row records
/// that something was seen, not how it turned out — so the question has no
/// answer rather than an unmeasured one.
///
/// Returning `measurable=false` was the right answer to a *data* gap (#1897):
/// an outcome-bearing panel that has not received its anchors yet will receive
/// them, and a zeroed report that says so is honest. This is a different
/// condition. Here the caller has asked a question the panel's own declaration
/// says is meaningless, and answering it with a zeroed report invites the reader
/// to conclude "this panel's lenses carry no signal about the outcome" from a
/// number that was never about an outcome at all. That is the same shape as the
/// #1953/#1958 failures: a value structurally determined by configuration,
/// presented as a measurement about the data.
///
/// Keyed on the catalog declaration rather than on the observed anchor count, so
/// the verdict is a pure function of `panel_version` and cannot switch on with
/// traffic.
fn refuse_outcome_query_on_observation_panel(
    operation: &str,
    params: &SynapseCalyxAssayParams,
) -> StorageResult<()> {
    let Some(entry) = constellations::panel_catalog_entry_for_version(params.panel_version) else {
        // An unknown panel version is not this check's business; the assay's own
        // panel resolution fails closed on it with a better message.
        return Ok(());
    };
    if entry.outcome_bearing {
        return Ok(());
    }
    let outcome_bearing: Vec<String> = constellations::builtin_panel_catalog()
        .into_iter()
        .filter(|candidate| candidate.outcome_bearing)
        .map(|candidate| format!("{}@{}", candidate.panel_name, candidate.panel_version))
        .collect();
    Err(StorageError::WriteFailed {
        cf_name: cf::CF_KV.to_owned(),
        detail: format!(
            "SYNAPSE_ASSAY_PANEL_HAS_NO_OUTCOME_AXIS: {operation} measures bits *about* a grounded \
             outcome, and panel {}@{} is declared outcome_bearing=false — its rows record that \
             something was observed, not how it turned out, so it carries no anchor of any kind \
             and never will. A zeroed report over this panel would read as 'these lenses carry no \
             signal about {}' when nothing was ever measured about an outcome. Ask this on a panel \
             that receives outcomes: {outcome_bearing:?}. To confirm the panel's state directly, \
             read `hygiene operation=grounding_gap` — its no_outcome_axis flag is the same fact.",
            entry.panel_name, entry.panel_version, params.anchor_kind
        ),
    })
}

fn grounding_anchor_to_calyx(anchor: GroundingAnchor) -> StorageResult<Anchor> {
    let kind_label = nonblank_owned(&anchor.kind_label, "grounding anchor kind_label")?;
    let source = nonblank_owned(&anchor.source, "grounding anchor source")?;
    let value = match anchor.value {
        GroundingAnchorValue::Bool(value) => AnchorValue::Bool(value),
        GroundingAnchorValue::Enum(value) => {
            AnchorValue::Enum(nonblank_owned(&value, "grounding anchor enum value")?)
        }
        GroundingAnchorValue::Text(value) => {
            AnchorValue::Text(nonblank_owned(&value, "grounding anchor text value")?)
        }
        GroundingAnchorValue::Number(value) if value.is_finite() => AnchorValue::Number(value),
        GroundingAnchorValue::Number(value) => {
            return Err(calyx_write_failed_detail(
                "calyx_anchors",
                format!("grounding anchor number value is non-finite: {value}"),
            ));
        }
    };
    let anchor = Anchor {
        kind: AnchorKind::Label(kind_label),
        value,
        source,
        observed_at: anchor.observed_at_ms,
        confidence: anchor.confidence,
    };
    anchor.validate_schema().map_err(|error| {
        calyx_write_failed_detail(
            "calyx_anchors",
            format!("grounding anchor schema invalid: {error}"),
        )
    })?;
    Ok(anchor)
}

fn prepare_grounding_anchor_sources(
    vault: &SynapseCalyxVault,
    sources: Vec<GroundingAnchorSource>,
) -> StorageResult<PreparedGroundingAnchorBatch> {
    let mut prepared = Vec::with_capacity(sources.len());
    let mut calyx_entries = Vec::with_capacity(sources.len());
    for source in sources {
        let panel =
            constellations::anchor_panel_for_source_row(source.source_cf, &source.source_key)?;
        let input_bytes = constellations::source_constellation_input_bytes(
            panel.input_mode,
            source.source_cf,
            &source.source_key,
            &source.raw_bytes,
        );
        let cx_id = vault.cx_id_for_input(&input_bytes, panel.panel_version);
        let anchor = grounding_anchor_to_calyx(source.anchor)?;
        calyx_entries.push((cx_id, vec![anchor.clone()]));
        prepared.push(PreparedGroundingAnchorSource {
            source_cf: source.source_cf,
            source_key: source.source_key,
            cx_id,
            anchor,
        });
    }
    Ok((prepared, calyx_entries))
}

/// The typed anchor write-conflict code as it reaches Synapse (#2072).
///
/// The Synapse-namespace form, because `crates/synapse-calyx/src/error_bridge.rs`
/// maps `CALYX_ASTER_ANCHOR_VALUE_CONFLICT` onto it — the bridge validator
/// refuses any PRD-18 code without a mapping, so this is the only form a caller
/// ever sees.
const CALYX_ANCHOR_VALUE_CONFLICT_CODE: &str = "SYNAPSE_CALYX_ASTER_ANCHOR_VALUE_CONFLICT";

/// Records the constellations a refused anchor batch left un-advanced (#2072
/// ask 3 and ask 4).
///
/// One structured record per BATCH, naming every affected identity, rather than
/// one `ERROR` per constellation per retry: the deployed daemon emitted 318
/// events across 106 constellations in under three minutes, and an operator
/// cannot act on that shape. The identities are named exactly — source CF,
/// source key, `cx_id`, anchor kind — because "an anchor somewhere did not
/// advance" is not a repairable statement.
///
/// Deliberately a log record and not a durable queue row: this runs inside the
/// vault closure of a failing write, and taking a second write path there would
/// make the failure handler able to fail. The repair that consumes this is the
/// panel-coverage census, which already finds ungrounded and stale-grounded rows
/// by physical scan and does not need to be told where to look.
fn record_anchor_conflict_identities(
    prepared: &[PreparedGroundingAnchorSource],
    error: &StorageError,
) {
    let identities: Vec<String> = prepared
        .iter()
        .map(|source| {
            format!(
                "cx_id={} kind={:?} source_cf={} source_key_hex={}",
                source.cx_id,
                source.anchor.kind,
                source.source_cf,
                constellations::hex_encode(&source.source_key),
            )
        })
        .collect();
    tracing::error!(
        code = "CALYX_GROUNDING_ANCHOR_BATCH_CONFLICT",
        conflict_code = CALYX_ANCHOR_VALUE_CONFLICT_CODE,
        affected_constellations = identities.len(),
        identities = ?identities,
        detail = %error,
        "a grounded anchor batch was refused as a WRITE CONFLICT, not as corruption: every named \
         constellation kept its previously stored anchor and none of them advanced. The vault is \
         not damaged and must not be restored from a snapshot; reconcile the disagreeing \
         observations at their writers"
    );
}

fn readback_grounding_anchor_batch(
    vault: &SynapseCalyxVault,
    prepared: &[PreparedGroundingAnchorSource],
) -> StorageResult<u64> {
    let mut readback_exact_match_count = 0_u64;
    for source in prepared {
        let rows = vault.scan_anchors_for_cx(source.cx_id).map_err(|source| {
            calyx_read_failed("calyx_anchors", "read back grounded anchors", &source)
        })?;
        let exact_matches = rows
            .iter()
            .filter(|row| row.anchor == source.anchor)
            .count();
        if exact_matches != 1 {
            return Err(calyx_write_failed_detail(
                "calyx_anchors",
                format!(
                    "grounded anchor batch readback mismatch for source_cf={} key_hex={} cx_id={}: expected exactly 1 matching anchor, found {exact_matches}; total anchors={}",
                    source.source_cf,
                    constellations::hex_encode(&source.source_key),
                    source.cx_id,
                    rows.len()
                ),
            ));
        }
        readback_exact_match_count = readback_exact_match_count.saturating_add(1);
    }
    Ok(readback_exact_match_count)
}

fn nonblank_owned(value: &str, field: &'static str) -> StorageResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(calyx_write_failed_detail(
            "calyx_anchors",
            format!("{field} must not be blank"),
        ));
    }
    Ok(trimmed.to_owned())
}

struct AnchorWriteReportInput<'a> {
    source_cf: &'static str,
    source_key: &'a [u8],
    raw_bytes: &'a [u8],
    panel_name: &'static str,
    panel_version: u32,
    cx_id: CxId,
    anchor: &'a Anchor,
    write: SynapseCalyxAnchorWriteReadback,
    readback_anchor_count: usize,
}

fn anchor_write_report(input: AnchorWriteReportInput<'_>) -> CalyxAnchorWriteReport {
    CalyxAnchorWriteReport {
        source_cf: input.source_cf.to_owned(),
        source_key_hex: constellations::hex_encode(input.source_key),
        source_value_sha256: constellations::sha256_hex(input.raw_bytes),
        panel_name: input.panel_name.to_owned(),
        panel_version: input.panel_version,
        cx_id: input.cx_id.to_string(),
        anchor_kind: anchor_kind_label(&input.anchor.kind),
        anchor_value: anchor_value_readback(&input.anchor.value),
        anchor_source: input.anchor.source.clone(),
        confidence: input.anchor.confidence,
        ledger_seq: input.write.ledger_seq,
        ledger_hash: input.write.ledger_hash,
        latest_seq: input.write.latest_seq,
        readback_anchor_count: u64::try_from(input.readback_anchor_count).unwrap_or(u64::MAX),
    }
}

fn anchor_batch_write_report(
    input: SynapseCalyxAnchorBatchWriteReadback,
    readback_exact_match_count: u64,
    duration_us: u64,
) -> CalyxAnchorBatchWriteReport {
    CalyxAnchorBatchWriteReport {
        requested_anchor_count: u64::try_from(input.anchor_count).unwrap_or(u64::MAX),
        written_anchor_count: u64::try_from(input.written_anchor_count).unwrap_or(u64::MAX),
        existing_anchor_count: u64::try_from(input.existing_anchor_count).unwrap_or(u64::MAX),
        readback_exact_match_count,
        ledger_seq: input.ledger_seq,
        ledger_hash: input.ledger_hash,
        latest_seq: input.latest_seq,
        duration_us,
    }
}

fn anchor_rows(cx_id: CxId, rows: Vec<SynapseCalyxAnchorReadback>) -> Vec<CalyxAnchorRow> {
    rows.into_iter()
        .map(|row| CalyxAnchorRow {
            key_hex: constellations::hex_encode(&row.key),
            cx_id: cx_id.to_string(),
            kind: anchor_kind_label(&row.anchor.kind),
            value: anchor_value_readback(&row.anchor.value),
            source: row.anchor.source,
            observed_at_ms: row.anchor.observed_at,
            confidence: row.anchor.confidence,
        })
        .collect()
}

fn anchor_kind_label(kind: &AnchorKind) -> String {
    match kind {
        AnchorKind::TestPass => "test_pass".to_owned(),
        AnchorKind::TieFormed => "tie_formed".to_owned(),
        AnchorKind::Thumbs => "thumbs".to_owned(),
        AnchorKind::Reward => "reward".to_owned(),
        AnchorKind::SpeakerMatch => "speaker_match".to_owned(),
        AnchorKind::StyleHold => "style_hold".to_owned(),
        AnchorKind::Recurrence => "recurrence".to_owned(),
        AnchorKind::Label(value) => format!("label:{value}"),
    }
}

fn anchor_value_readback(value: &AnchorValue) -> CalyxAnchorValueReadback {
    match value {
        AnchorValue::Bool(value) => CalyxAnchorValueReadback {
            value_type: "bool".to_owned(),
            bool_value: Some(*value),
            text_value: None,
            number_value: None,
            one_hot_values: Vec::new(),
            vector_len: None,
            vector_sha256: None,
        },
        AnchorValue::Enum(value) => CalyxAnchorValueReadback {
            value_type: "enum".to_owned(),
            bool_value: None,
            text_value: Some(value.clone()),
            number_value: None,
            one_hot_values: Vec::new(),
            vector_len: None,
            vector_sha256: None,
        },
        AnchorValue::Number(value) => CalyxAnchorValueReadback {
            value_type: "number".to_owned(),
            bool_value: None,
            text_value: None,
            number_value: Some(*value),
            one_hot_values: Vec::new(),
            vector_len: None,
            vector_sha256: None,
        },
        AnchorValue::OneHot(values) => CalyxAnchorValueReadback {
            value_type: "one_hot".to_owned(),
            bool_value: None,
            text_value: None,
            number_value: None,
            one_hot_values: values.clone(),
            vector_len: None,
            vector_sha256: None,
        },
        AnchorValue::Text(value) => CalyxAnchorValueReadback {
            value_type: "text".to_owned(),
            bool_value: None,
            text_value: Some(value.clone()),
            number_value: None,
            one_hot_values: Vec::new(),
            vector_len: None,
            vector_sha256: None,
        },
        AnchorValue::Vector(values) => {
            let mut bytes = Vec::with_capacity(values.len().saturating_mul(4));
            for value in values {
                bytes.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            CalyxAnchorValueReadback {
                value_type: "vector".to_owned(),
                bool_value: None,
                text_value: None,
                number_value: None,
                one_hot_values: Vec::new(),
                vector_len: Some(u64::try_from(values.len()).unwrap_or(u64::MAX)),
                vector_sha256: Some(sha256_hex(&bytes)),
            }
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn emit_storage_cf_bytes(sizes: &BTreeMap<String, u64>) {
    for (cf_name, bytes) in sizes {
        synapse_telemetry::metrics::gauge!(STORAGE_CF_BYTES, "cf" => cf_name.clone())
            .set(*bytes as f64);
    }
}

fn verify_calyx_schema_version(
    vault: &SynapseCalyxVault,
    path: &Path,
    schema_version: u32,
) -> StorageResult<()> {
    let key = encode_calyx_legacy_key(CALYX_METADATA_COLLECTION_ID, SCHEMA_VERSION_KEY)
        .map_err(|detail| calyx_open_failed_detail(path, detail))?;
    let existing = vault
        .read_cf_latest(ColumnFamily::Kv, &key)
        .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?;
    match existing {
        None => {
            let row = SynapseCalyxCfWrite::new(
                ColumnFamily::Kv,
                key,
                encode_calyx_value(0, 0, &schema_version.to_be_bytes()),
            );
            vault
                .write_cf_batch(vec![row])
                .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?;
            vault
                .flush()
                .map_err(|source| calyx_open_failed_detail(path, source.to_string()))
        }
        Some(value) => {
            let payload = decode_calyx_value_raw(&value)
                .map_err(|detail| calyx_open_failed_detail(path, detail))?;
            let actual = decode_schema_version(payload.payload);
            if actual == Some(schema_version) {
                Ok(())
            } else {
                Err(StorageError::SchemaMismatch {
                    expected: schema_version,
                    actual: actual.unwrap_or_default(),
                })
            }
        }
    }
}

fn verify_calyx_schema_version_existing(
    vault: &impl CalyxVaultKvRead,
    path: &Path,
    expected_schema_version: u32,
) -> StorageResult<u32> {
    let key = encode_calyx_legacy_key(CALYX_METADATA_COLLECTION_ID, SCHEMA_VERSION_KEY)
        .map_err(|detail| calyx_open_failed_detail(path, detail))?;
    let Some(value) = vault
        .read_kv_latest(&key)
        .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?
    else {
        return Err(calyx_open_failed_detail(
            path,
            "missing Synapse schema-version sentinel in Calyx metadata collection".to_owned(),
        ));
    };
    let payload =
        decode_calyx_value_raw(&value).map_err(|detail| calyx_open_failed_detail(path, detail))?;
    let actual = decode_schema_version(payload.payload).unwrap_or_default();
    if expected_schema_version == 0 || actual == expected_schema_version {
        Ok(actual)
    } else {
        Err(StorageError::SchemaMismatch {
            expected: expected_schema_version,
            actual,
        })
    }
}

fn ordered_key_migration_sentinel_key(path: &Path) -> StorageResult<Vec<u8>> {
    encode_calyx_legacy_key(
        CALYX_METADATA_COLLECTION_ID,
        CALYX_ORDERED_KEY_MIGRATION_KEY,
    )
    .map_err(|detail| calyx_open_failed_detail(path, detail))
}

fn read_calyx_ordered_key_migration_sentinel(
    vault: &impl CalyxVaultKvRead,
    path: &Path,
) -> StorageResult<bool> {
    let key = ordered_key_migration_sentinel_key(path)?;
    let Some(value) = vault
        .read_kv_latest(&key)
        .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?
    else {
        return Ok(false);
    };
    let envelope =
        decode_calyx_value_raw(&value).map_err(|detail| calyx_open_failed_detail(path, detail))?;
    if envelope.expires_at_ms != 0 || envelope.payload != CALYX_ORDERED_KEY_MIGRATION_PAYLOAD {
        return Err(calyx_open_failed_detail(
            path,
            format!(
                "CALYX_ORDERED_KEY_MIGRATION_SENTINEL_CORRUPT: key={} expires_at_ms={} payload_sha256={}; remediation=inspect the physical metadata row and the legacy/ordered namespace counts before exact repair",
                hex_prefix_for_log(&key),
                envelope.expires_at_ms,
                sha256_hex(envelope.payload)
            ),
        ));
    }
    Ok(true)
}

fn verify_calyx_legacy_namespaces_empty(
    vault: &impl CalyxVaultKvRead,
    path: &Path,
) -> StorageResult<()> {
    for cf_name in cf::ALL_COLUMN_FAMILIES {
        let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
        let range = prefix_range(&calyx_legacy_namespace_prefix(collection_id));
        let rows = vault.scan_kv_range_latest(&range).map_err(|source| {
            calyx_open_failed_detail(
                path,
                format!("verify empty legacy Calyx namespace for {cf_name}: {source}"),
            )
        })?;
        if let Some((physical_key, _value)) = rows.first() {
            let user_key = decode_calyx_legacy_user_key(collection_id, physical_key).map_err(
                |detail| {
                    calyx_open_failed_detail(
                        path,
                        format!(
                            "CALYX_ORDERED_KEY_LEGACY_NAMESPACE_CORRUPT: cf={cf_name} physical_key={} detail={detail}",
                            hex_prefix_for_log(physical_key)
                        ),
                    )
                },
            )?;
            return Err(calyx_open_failed_detail(
                path,
                format!(
                    "CALYX_ORDERED_KEY_MIGRATION_INCOMPLETE: completion sentinel exists while a live legacy row remains: cf={cf_name} user_key_len={} user_key_sha256={}; remediation=remove the invalid sentinel only after preserving this evidence, then reopen the repo-built daemon so the resumable migration finishes",
                    user_key.len(),
                    sha256_hex(&user_key)
                ),
            ));
        }
    }
    Ok(())
}

fn require_calyx_ordered_key_migration(
    vault: &impl CalyxVaultKvRead,
    path: &Path,
) -> StorageResult<()> {
    if !read_calyx_ordered_key_migration_sentinel(vault, path)? {
        return Err(calyx_open_failed_detail(
            path,
            "CALYX_ORDERED_KEY_MIGRATION_REQUIRED: the read-only vault has no ordered-key migration sentinel; remediation=stop readers and open it once with the repo-built Synapse writer to complete the resumable migration"
                .to_owned(),
        ));
    }
    verify_calyx_legacy_namespaces_empty(vault, path)
}

#[derive(Clone, Copy, Debug, Default)]
struct CalyxOrderedKeyMigrationCounts {
    migrated_rows: u64,
    resumed_rows: u64,
    committed_pages: u64,
}

impl CalyxOrderedKeyMigrationCounts {
    const fn add(&mut self, other: Self) {
        self.migrated_rows = self.migrated_rows.saturating_add(other.migrated_rows);
        self.resumed_rows = self.resumed_rows.saturating_add(other.resumed_rows);
        self.committed_pages = self.committed_pages.saturating_add(other.committed_pages);
    }
}

fn prepare_calyx_ordered_migration_page(
    vault: &SynapseCalyxVault,
    path: &Path,
    cf_name: &str,
    collection_id: u64,
    rows: SynapseCalyxCfRows,
) -> StorageResult<(Vec<SynapseCalyxCfWrite>, CalyxOrderedKeyMigrationCounts)> {
    let mut writes = Vec::with_capacity(rows.len().saturating_mul(2));
    let mut counts = CalyxOrderedKeyMigrationCounts::default();
    for (legacy_key, stored_value) in rows {
        let user_key = decode_calyx_legacy_user_key(collection_id, &legacy_key).map_err(
            |detail| {
                calyx_open_failed_detail(
                    path,
                    format!(
                        "CALYX_ORDERED_KEY_LEGACY_ROW_CORRUPT: cf={cf_name} physical_key={} detail={detail}",
                        hex_prefix_for_log(&legacy_key)
                    ),
                )
            },
        )?;
        let ordered_key = encode_calyx_key(collection_id, &user_key).map_err(|detail| {
            calyx_open_failed_detail(
                path,
                format!(
                    "encode ordered Calyx key during migration: cf={cf_name} user_key_len={} detail={detail}",
                    user_key.len()
                ),
            )
        })?;
        match vault
            .read_cf_latest(ColumnFamily::Kv, &ordered_key)
            .map_err(|source| {
                calyx_open_failed_detail(
                    path,
                    format!("read ordered Calyx migration target for {cf_name}: {source}"),
                )
            })? {
            Some(existing) if existing != stored_value => {
                return Err(calyx_open_failed_detail(
                    path,
                    format!(
                        "CALYX_ORDERED_KEY_MIGRATION_DIVERGED: cf={cf_name} user_key_len={} user_key_sha256={} legacy_value_sha256={} ordered_value_sha256={}; remediation=quarantine and reconcile the exact physical pair before reopening",
                        user_key.len(),
                        sha256_hex(&user_key),
                        sha256_hex(&stored_value),
                        sha256_hex(&existing)
                    ),
                ));
            }
            Some(_) => counts.resumed_rows = counts.resumed_rows.saturating_add(1),
            None => {
                writes.push(SynapseCalyxCfWrite::new(
                    ColumnFamily::Kv,
                    ordered_key,
                    stored_value,
                ));
                counts.migrated_rows = counts.migrated_rows.saturating_add(1);
            }
        }
        writes.push(SynapseCalyxCfWrite::new(
            ColumnFamily::Kv,
            legacy_key,
            tombstone_value(),
        ));
    }
    Ok((writes, counts))
}

fn migrate_calyx_legacy_cf_ordered(
    vault: &SynapseCalyxVault,
    path: &Path,
    cf_name: &str,
) -> StorageResult<CalyxOrderedKeyMigrationCounts> {
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_legacy_namespace_prefix(collection_id));
    let mut after_physical = None;
    let mut counts = CalyxOrderedKeyMigrationCounts::default();
    loop {
        let page = vault
            .scan_cf_range_page_latest(
                ColumnFamily::Kv,
                &range,
                after_physical.as_deref(),
                CALYX_ORDERED_KEY_MIGRATION_PAGE_ROWS,
            )
            .map_err(|source| {
                calyx_open_failed_detail(
                    path,
                    format!("scan resumable legacy Calyx namespace page for {cf_name}: {source}"),
                )
            })?;
        let next_after = page.resume_after;
        let more = page.more;
        let (writes, page_counts) =
            prepare_calyx_ordered_migration_page(vault, path, cf_name, collection_id, page.rows)?;
        counts.add(page_counts);
        if !writes.is_empty() {
            commit_calyx_rows_to_vault(vault, cf_name, writes)?;
            counts.committed_pages = counts.committed_pages.saturating_add(1);
        }
        if !more {
            return Ok(counts);
        }
        after_physical = Some(next_after.ok_or_else(|| {
            calyx_open_failed_detail(
                path,
                format!(
                    "CALYX_ORDERED_KEY_MIGRATION_CURSOR_MISSING: legacy {cf_name} page reported more candidates without an exclusive physical cursor"
                ),
            )
        })?);
    }
}

fn commit_calyx_ordered_migration_sentinel(
    vault: &SynapseCalyxVault,
    path: &Path,
) -> StorageResult<()> {
    let sentinel_key = ordered_key_migration_sentinel_key(path)?;
    let sentinel_value = encode_calyx_value(0, 0, CALYX_ORDERED_KEY_MIGRATION_PAYLOAD);
    commit_calyx_rows_atomically_to_vault(
        vault,
        "<ordered-key-migration>",
        vec![SynapseCalyxCfWrite::new(
            ColumnFamily::Kv,
            sentinel_key.clone(),
            sentinel_value.clone(),
        )],
    )?;
    let readback = vault
        .read_cf_latest(ColumnFamily::Kv, &sentinel_key)
        .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?;
    if readback.as_deref() != Some(sentinel_value.as_slice()) {
        return Err(calyx_open_failed_detail(
            path,
            format!(
                "CALYX_ORDERED_KEY_MIGRATION_SENTINEL_READBACK_MISMATCH: key={} expected_sha256={} actual_sha256={}; remediation=inspect WAL/MVCC publication before retrying open",
                hex_prefix_for_log(&sentinel_key),
                sha256_hex(&sentinel_value),
                readback
                    .as_deref()
                    .map_or_else(|| "absent".to_owned(), sha256_hex)
            ),
        ));
    }
    Ok(())
}

fn ordered_key_physical_purge_sentinel_key(path: &Path) -> StorageResult<Vec<u8>> {
    encode_calyx_legacy_key(
        CALYX_METADATA_COLLECTION_ID,
        CALYX_ORDERED_KEY_PHYSICAL_PURGE_KEY,
    )
    .map_err(|detail| calyx_open_failed_detail(path, detail))
}

fn read_calyx_ordered_key_physical_purge_sentinel(
    vault: &impl CalyxVaultKvRead,
    path: &Path,
) -> StorageResult<bool> {
    let key = ordered_key_physical_purge_sentinel_key(path)?;
    let Some(value) = vault
        .read_kv_latest(&key)
        .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?
    else {
        return Ok(false);
    };
    let envelope =
        decode_calyx_value_raw(&value).map_err(|detail| calyx_open_failed_detail(path, detail))?;
    if envelope.expires_at_ms != 0 || envelope.payload != CALYX_ORDERED_KEY_PHYSICAL_PURGE_PAYLOAD {
        return Err(calyx_open_failed_detail(
            path,
            format!(
                "CALYX_ORDERED_KEY_PHYSICAL_PURGE_SENTINEL_CORRUPT: key={} expires_at_ms={} payload_sha256={}; remediation=preserve the KV SST inventory and inspect the exact metadata row before repair",
                hex_prefix_for_log(&key),
                envelope.expires_at_ms,
                sha256_hex(envelope.payload)
            ),
        ));
    }
    Ok(true)
}

fn commit_calyx_ordered_key_physical_purge_sentinel(
    vault: &SynapseCalyxVault,
    path: &Path,
) -> StorageResult<()> {
    let key = ordered_key_physical_purge_sentinel_key(path)?;
    let value = encode_calyx_value(0, 0, CALYX_ORDERED_KEY_PHYSICAL_PURGE_PAYLOAD);
    commit_calyx_rows_atomically_to_vault(
        vault,
        "<ordered-key-physical-purge>",
        vec![SynapseCalyxCfWrite::new(
            ColumnFamily::Kv,
            key.clone(),
            value.clone(),
        )],
    )?;
    let readback = vault
        .read_cf_latest(ColumnFamily::Kv, &key)
        .map_err(|source| calyx_open_failed_detail(path, source.to_string()))?;
    if readback.as_deref() != Some(value.as_slice()) {
        return Err(calyx_open_failed_detail(
            path,
            format!(
                "CALYX_ORDERED_KEY_PHYSICAL_PURGE_SENTINEL_READBACK_MISMATCH: key={} expected_sha256={} actual_sha256={}; remediation=inspect the WAL commit and latest KV readback before retrying open",
                hex_prefix_for_log(&key),
                sha256_hex(&value),
                readback
                    .as_deref()
                    .map_or_else(|| "absent".to_owned(), sha256_hex)
            ),
        ));
    }
    Ok(())
}

fn ensure_calyx_ordered_key_physical_purge(
    vault: &SynapseCalyxVault,
    path: &Path,
) -> StorageResult<()> {
    if read_calyx_ordered_key_physical_purge_sentinel(vault, path)? {
        return Ok(());
    }
    let started = Instant::now();
    vault.purge_kv_tombstones().map_err(|source| {
        calyx_open_failed_detail(
            path,
            format!(
                "physically purge retired ordered-key migration tombstones with a complete KV compaction: {source}"
            ),
        )
    })?;
    verify_calyx_legacy_namespaces_empty(vault, path)?;
    commit_calyx_ordered_key_physical_purge_sentinel(vault, path)?;
    tracing::info!(
        code = "STORAGE_CALYX_ORDERED_KEY_PHYSICAL_PURGE_COMPLETED",
        latest_seq = vault.latest_seq(),
        elapsed_ms = started.elapsed().as_millis(),
        "physically removed retired ordered-key migration tombstones through a complete KV compaction and verified its durable sentinel"
    );
    Ok(())
}

fn ensure_calyx_ordered_key_migration(vault: &SynapseCalyxVault, path: &Path) -> StorageResult<()> {
    if read_calyx_ordered_key_migration_sentinel(vault, path)? {
        verify_calyx_legacy_namespaces_empty(vault, path)?;
        return ensure_calyx_ordered_key_physical_purge(vault, path);
    }

    let started = Instant::now();
    let mut counts = CalyxOrderedKeyMigrationCounts::default();
    for cf_name in cf::ALL_COLUMN_FAMILIES {
        counts.add(migrate_calyx_legacy_cf_ordered(vault, path, cf_name)?);
    }
    verify_calyx_legacy_namespaces_empty(vault, path)?;
    commit_calyx_ordered_migration_sentinel(vault, path)?;
    tracing::info!(
        code = "STORAGE_CALYX_ORDERED_KEY_MIGRATION_COMPLETED",
        migrated_rows = counts.migrated_rows,
        resumed_rows = counts.resumed_rows,
        committed_pages = counts.committed_pages,
        latest_seq = vault.latest_seq(),
        elapsed_ms = started.elapsed().as_millis(),
        "migrated every live logical storage row into the order-preserving Calyx namespace and verified the durable sentinel"
    );
    ensure_calyx_ordered_key_physical_purge(vault, path)
}

fn decode_schema_version(bytes: &[u8]) -> Option<u32> {
    <[u8; 4]>::try_from(bytes).ok().map(u32::from_be_bytes)
}

/// Rows per page for every bounded-hold inspection sweep (#2041).
///
/// Deliberately the same value `synapse_calyx` swept for
/// [`synapse_calyx::SYNAPSE_CALYX_CF_WALK_PAGE_ROWS`] rather than a second,
/// independently-guessed constant: that sweep measured the worst single
/// row-table read-guard hold against `ROW_READ_GUARD_WARN_US` (25,000 us) at
/// six page sizes on the real vault and found 256 lands the worst hold at 16%
/// of budget, with a budget cliff between 2,048 and 4,096. Two constants would
/// drift apart and one of them would silently be the wrong side of that cliff.
const CALYX_INSPECT_SWEEP_PAGE_ROWS: usize = synapse_calyx::SYNAPSE_CALYX_CF_WALK_PAGE_ROWS;

/// Sweep label for the panel-coverage census's per-source-CF fold (#2060).
///
/// A `site` label on `STORAGE_CALYX_BOUNDED_SWEEP`, not a new
/// `calyx_row_guard_sites` entry, for the reason
/// [`CALYX_GC_RETENTION_SWEEP_SITE`] documents: the physical stream reports its
/// existing snapshot-page guard site, while this label attributes the complete
/// logical operation without minting a second low-level metric identity.
const PANEL_COVERAGE_SOURCE_CENSUS_SITE: &str = "panel_coverage_source_census";

/// Provenance of one bounded-hold sweep over an ordered Calyx KV range (#2041).
///
/// Each individual sweep now owns one registered snapshot and therefore must
/// report the same first and last sequence. The fields remain explicit because
/// [`CalyxKvSweep::merge`] combines independently pinned per-family scans; that
/// aggregate is an interval when the families were captured at different
/// commits, and must never be presented as one instant.
///
/// This mirrors [`synapse_calyx::SynapseCalyxCfWalk`] exactly, and exists
/// separately only because these sweeps are *range*-scoped: all 17 Synapse
/// column families share the one physical `Kv` family and are distinguished by
/// an ordered namespace prefix, so a whole-family walk would read 17x the vault
/// to answer one family's question.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CalyxKvSweep {
    pages: usize,
    rows_examined: usize,
    rows_visited: usize,
    snapshot_seq_first: u64,
    snapshot_seq_last: u64,
    stopped_early: bool,
}

impl CalyxKvSweep {
    /// Whether every page was served by the same committed sequence.
    const fn atomic(&self) -> bool {
        self.pages > 0 && self.snapshot_seq_first == self.snapshot_seq_last
    }

    const fn merge(&mut self, other: Self) {
        if self.pages == 0 {
            self.snapshot_seq_first = other.snapshot_seq_first;
        }
        if other.pages > 0 {
            self.snapshot_seq_last = other.snapshot_seq_last;
        }
        self.pages = self.pages.saturating_add(other.pages);
        self.rows_examined = self.rows_examined.saturating_add(other.rows_examined);
        self.rows_visited = self.rows_visited.saturating_add(other.rows_visited);
        self.stopped_early |= other.stopped_early;
    }
}

/// Folds one ordered Calyx KV range page by page, releasing the vault's
/// row-table read guard between pages (#2041).
///
/// **This is the whole of #2041.** `scan_kv_range_latest` answers the same
/// question under a *single* acquisition of the MVCC row-table read guard held
/// for as long as it takes to merge and materialise the entire range — and that
/// guard is the one every vault write must take exclusively, so a diagnostic
/// read of a 1,025,928-row vault stalled unrelated MCP work for seconds. The
/// live daemon recorded 184 `CALYX_ASTER_ROW_READ_GUARD_SLOW` events, every one
/// of them `site="scan_cf_range_latest"`, holds commonly 27-57 ms, against a
/// 25 ms budget.
///
/// The first paged implementation bounded the hold but reopened and re-sought
/// every immutable file for every 256-row handoff. GC consequently accumulated
/// those cursor transients and crossed 1 GiB. The current implementation keeps
/// one sequential cursor and one registered snapshot for the whole range,
/// releases each returned page promptly, then destroys and measures the cursor
/// at the outer ownership boundary.
///
/// # Errors
///
/// Fails closed when snapshot registration, lease validation, immutable record
/// validation, allocator accounting, or the visitor fails. The visitor may
/// return [`ControlFlow::Break`] after consuming a row to stop without reading
/// another page; its own error is preserved verbatim, including alongside an
/// independent stream-release failure.
fn sweep_kv_range_pages<V>(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    site: &'static str,
    range: &KeyRange,
    mut visit: V,
) -> StorageResult<CalyxKvSweep>
where
    V: FnMut(&[u8], &[u8]) -> StorageResult<ControlFlow<()>>,
{
    let started = Instant::now();
    let mut visitor_error = None;
    let walk_result =
        vault.walk_kv_range_latest_snapshot(range, CALYX_INSPECT_SWEEP_PAGE_ROWS, |key, value| {
            match visit(key, value) {
                Ok(ControlFlow::Continue(())) => Ok(SynapseCalyxWalkStep::Continue),
                Ok(ControlFlow::Break(())) => Ok(SynapseCalyxWalkStep::Stop),
                Err(error) => {
                    visitor_error = Some(error);
                    Ok(SynapseCalyxWalkStep::Stop)
                }
            }
        });
    let walk = match (walk_result, visitor_error) {
        (Ok(_), Some(error)) => return Err(error),
        (Err(stream_error), Some(visitor_error)) => {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "CALYX_STREAM_AND_VISITOR_FAILED: the {site} visitor failed with {visitor_error}; after stopping it, the persistent KV stream also failed with {stream_error}; remediation=repair both independently reported failures before retrying"
                ),
            });
        }
        (Err(source), None) => {
            return Err(calyx_read_failed(
                cf_name,
                "stream one pinned Calyx KV inspection range",
                &source,
            ));
        }
        (Ok(walk), None) => walk,
    };
    if !walk.atomic() {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "CALYX_PINNED_STREAM_SEQUENCE_DRIFTED: the {site} stream reported pages={} first_seq={} last_seq={}; one registered snapshot must serve the complete range; remediation=repair the persistent snapshot stream before trusting this census",
                walk.pages, walk.snapshot_seq_first, walk.snapshot_seq_last
            ),
        });
    }
    let sweep = CalyxKvSweep {
        pages: walk.pages,
        rows_examined: walk.rows_examined,
        rows_visited: walk.rows_visited,
        snapshot_seq_first: walk.snapshot_seq_first,
        snapshot_seq_last: walk.snapshot_seq_last,
        stopped_early: walk.stopped_early,
    };
    tracing::debug!(
        code = "STORAGE_CALYX_BOUNDED_SWEEP",
        site,
        cf = cf_name,
        pages = sweep.pages,
        page_rows = CALYX_INSPECT_SWEEP_PAGE_ROWS,
        rows_visited = sweep.rows_visited,
        rows_examined = sweep.rows_examined,
        elapsed_ms = started.elapsed().as_millis(),
        snapshot_seq_first = sweep.snapshot_seq_first,
        snapshot_seq_last = sweep.snapshot_seq_last,
        atomic = sweep.atomic(),
        stopped_early = sweep.stopped_early,
        "folded an ordered Calyx KV range through one pinned persistent stream, releasing each page and all cursor ownership at the exact operation boundary"
    );
    Ok(sweep)
}

/// [`sweep_kv_range_pages`] over one logical column family's ordered namespace,
/// decoding each row exactly as [`read_rows_from_vault_range_filtered`] does and
/// handing the visitor only the live (non-expired) rows.
///
/// The strictly-increasing logical-key check is carried across page boundaries
/// rather than dropped: the unpaged reader asserted it over the materialised
/// vector, and losing that assertion is exactly how a paged rewrite silently
/// stops detecting a duplicate or corrupt namespace-one key.
fn sweep_calyx_namespace_live_rows<V>(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    site: &'static str,
    mut visit: V,
) -> StorageResult<CalyxKvSweep>
where
    V: FnMut(&[u8], &[u8]) -> StorageResult<()>,
{
    sweep_calyx_namespace_rows(vault, cf_name, site, |key, payload, expired| {
        if expired {
            return Ok(());
        }
        visit(key, payload)
    })
}

/// [`sweep_calyx_namespace_live_rows`], but the visitor also learns whether each
/// row is past its TTL instead of never seeing it.
///
/// The two questions the panel-coverage census asks of a source CF differ by
/// exactly this flag (#1940): the coverage denominator counts rows worth
/// measuring, so an expired row is excluded, while the orphan probe asks whether
/// a record's own source row can still be *read*, and an expired-but-present row
/// can. Reading the CF twice to answer them would double the fold; carrying the
/// flag answers both from one pass, with the same bounded holds.
fn sweep_calyx_namespace_rows<V>(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    site: &'static str,
    mut visit: V,
) -> StorageResult<CalyxKvSweep>
where
    V: FnMut(&[u8], &[u8], bool) -> StorageResult<()>,
{
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    sweep_calyx_range_rows(vault, cf_name, site, &range, |key, payload, expired| {
        visit(key, payload, expired)?;
        Ok(ControlFlow::Continue(()))
    })
}

/// Exact ordered source-key membership with one allocation for all key bytes and
/// one fixed-width offset per key.
///
/// The logical source namespace is strictly ordered by
/// [`sweep_calyx_namespace_rows`]. Keeping raw bytes preserves that ordering and
/// avoids materialising two lowercase hex bytes plus a `String` allocation for
/// every physical row. Candidate Base metadata is compared to the lowercase hex
/// representation byte by byte, without decoding or allocating it; malformed or
/// uppercase metadata therefore remains absent exactly as it was under the old
/// lowercase-String merge.
#[derive(Default)]
struct PackedSortedKeys {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl PackedSortedKeys {
    fn push(&mut self, cf_name: &str, key: &[u8]) -> StorageResult<()> {
        if let Some(previous) = self.last()
            && previous >= key
        {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "SYNAPSE_PANEL_COVERAGE_SOURCE_INDEX_ORDER_INVALID: source keys must be strictly increasing; previous={} current={}",
                    constellations::hex_encode(previous),
                    constellations::hex_encode(key),
                ),
            });
        }
        let end = self.bytes.len().checked_add(key.len()).ok_or_else(|| {
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "SYNAPSE_PANEL_COVERAGE_SOURCE_INDEX_SIZE_OVERFLOW: packed key byte length overflowed usize while appending a {}-byte key",
                    key.len()
                ),
            }
        })?;
        let end = u32::try_from(end).map_err(|_| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "SYNAPSE_PANEL_COVERAGE_SOURCE_INDEX_TOO_LARGE: exact packed source keys require {end} bytes, exceeding the u32 offset representation; reduce the physical source corpus itself or widen the persisted index representation before retrying"
            ),
        })?;
        self.bytes.extend_from_slice(key);
        self.ends.push(end);
        Ok(())
    }

    const fn len(&self) -> usize {
        self.ends.len()
    }

    const fn packed_bytes(&self) -> usize {
        self.bytes.len()
    }

    const fn offset_bytes(&self) -> usize {
        self.ends.len() * size_of::<u32>()
    }

    fn key(&self, index: usize) -> &[u8] {
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous] as usize);
        let end = self.ends[index] as usize;
        &self.bytes[start..end]
    }

    fn last(&self) -> Option<&[u8]> {
        (!self.ends.is_empty()).then(|| self.key(self.ends.len() - 1))
    }

    fn contains_lower_hex(&self, candidate: &str) -> bool {
        let mut left = 0_usize;
        let mut right = self.ends.len();
        while left < right {
            let middle = left + (right - left) / 2;
            match cmp_raw_key_to_lower_hex(self.key(middle), candidate.as_bytes()) {
                CmpOrdering::Less => left = middle + 1,
                CmpOrdering::Greater => right = middle,
                CmpOrdering::Equal => return true,
            }
        }
        false
    }
}

fn panel_coverage_declared_source_cfs() -> BTreeMap<String, bool> {
    let mut declared = BTreeMap::new();
    for entry in constellations::builtin_panel_catalog() {
        if let Some(cf_name) = entry.source.cf_name() {
            declared
                .entry(cf_name.to_owned())
                .and_modify(|full_cf| *full_cf |= entry.source.is_full_cf())
                .or_insert_with(|| entry.source.is_full_cf());
        }
    }
    declared
}

fn cmp_raw_key_to_lower_hex(raw: &[u8], candidate: &[u8]) -> CmpOrdering {
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    let encoded_len = raw.len().saturating_mul(2);
    let shared_len = encoded_len.min(candidate.len());
    for index in 0..shared_len {
        let byte = raw[index / 2];
        let encoded = if index % 2 == 0 {
            LOWER_HEX[usize::from(byte >> 4)]
        } else {
            LOWER_HEX[usize::from(byte & 0x0f)]
        };
        match encoded.cmp(&candidate[index]) {
            CmpOrdering::Equal => {}
            ordering => return ordering,
        }
    }
    encoded_len.cmp(&candidate.len())
}

/// Builds one [`PackedSortedKeys`] from the exact physical source namespace.
/// Expired rows remain present for membership but not for the live-row coverage
/// denominator.
fn build_panel_coverage_source_index(
    reader: &CalyxPinnedReader<'_>,
    cf_name: &str,
    read_at_unix_ms: u64,
) -> StorageResult<(u64, PackedSortedKeys)> {
    let mut live_rows = 0_u64;
    let mut index = PackedSortedKeys::default();
    sweep_calyx_namespace_rows_pinned(
        reader,
        cf_name,
        read_at_unix_ms,
        |key, _payload, expired| {
            index.push(cf_name, key)?;
            if !expired {
                live_rows = live_rows.saturating_add(1);
            }
            Ok(())
        },
    )?;
    Ok((live_rows, index))
}

/// Pinned-sequence counterpart of [`sweep_calyx_namespace_rows`]. It preserves
/// the same key/value decoding, TTL rule, and strict logical-order assertion,
/// while every source namespace and the later Base census share one committed
/// sequence.
fn sweep_calyx_namespace_rows_pinned<V>(
    reader: &CalyxPinnedReader<'_>,
    cf_name: &str,
    read_at_unix_ms: u64,
    mut visit: V,
) -> StorageResult<CalyxPinnedCfWalk>
where
    V: FnMut(&[u8], &[u8], bool) -> StorageResult<()>,
{
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let mut previous: Option<Vec<u8>> = None;
    walk_cf_range_pages_pinned(reader, ColumnFamily::Kv, &range, |key, value| {
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, key)?;
        let envelope = decode_calyx_value_raw(value).map_err(|detail| {
            tracing::error!(
                code = error_codes::STORAGE_READ_FAILED,
                cf = cf_name,
                detail,
                pinned_seq = reader.pinned_seq(),
                "pinned panel-coverage census rejected a malformed KV retention envelope"
            );
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail,
            }
        })?;
        if previous
            .as_deref()
            .is_some_and(|earlier| user_key.as_slice() <= earlier)
        {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "CALYX_ORDERED_KEY_RANGE_OUT_OF_ORDER: pinned sequence {} did not decode into strictly increasing logical keys; previous={} current={}; remediation=inspect duplicate/corrupt namespace-one keys before retrying",
                    reader.pinned_seq(),
                    previous
                        .as_deref()
                        .map_or_else(String::new, constellations::hex_encode),
                    constellations::hex_encode(&user_key),
                ),
            });
        }
        let expired = calyx_value_is_expired(envelope.expires_at_ms, read_at_unix_ms);
        visit(&user_key, envelope.payload, expired)?;
        previous = Some(user_key);
        Ok(())
    })
}

/// [`sweep_calyx_namespace_rows`] over an arbitrary ordered sub-range of one
/// column family's namespace.
///
/// Split out so the ordinary ordered-range read path (`scan_cf`,
/// `scan_cf_prefix*`, `scan_cf_from`) shares exactly this decode, this TTL rule
/// and this ordering assertion with the maintenance sweeps instead of keeping a
/// second, unpaged copy of them (#2060).
fn sweep_calyx_range_rows<V>(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    site: &'static str,
    range: &KeyRange,
    mut visit: V,
) -> StorageResult<CalyxKvSweep>
where
    V: FnMut(&[u8], &[u8], bool) -> StorageResult<ControlFlow<()>>,
{
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    let mut previous: Option<Vec<u8>> = None;
    sweep_kv_range_pages(vault, cf_name, site, range, |key, value| {
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, key)?;
        let envelope = decode_calyx_value_raw(value).map_err(|detail| {
            tracing::error!(
                code = error_codes::STORAGE_READ_FAILED,
                cf = cf_name,
                detail,
                "Calyx storage backend rejected malformed KV retention envelope"
            );
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail,
            }
        })?;
        let expired = calyx_value_is_expired(envelope.expires_at_ms, now_ms);
        // The ordering assertion is taken over EVERY decoded key, including
        // expired ones. Checking only the visited subset would stop detecting a
        // duplicate or corrupt namespace-one key the moment its neighbour
        // expired, which is precisely when a corrupt key is hardest to see.
        if previous
            .as_deref()
            .is_some_and(|earlier| user_key.as_slice() <= earlier)
        {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: "CALYX_ORDERED_KEY_RANGE_OUT_OF_ORDER: physical ordered namespace did not decode into strictly increasing logical keys; remediation=inspect duplicate/corrupt namespace-one keys before retrying"
                    .to_owned(),
            });
        }
        let control = visit(&user_key, envelope.payload, expired)?;
        previous = Some(user_key);
        Ok(control)
    })
}

/// One bounded-hold pass over every Synapse column family, reported as a whole.
///
/// The per-family sweep is already `debug`; this is the one `info` record a
/// multi-family diagnostic emits, so a future contention report can attribute a
/// whole `storage inspect` to a call site without reconstructing it from 17
/// separate lines.
fn sweep_every_calyx_namespace<V>(
    vault: &impl CalyxVaultKvRead,
    site: &'static str,
    mut visit: V,
) -> StorageResult<()>
where
    V: FnMut(&'static str, &[u8], &[u8]) -> StorageResult<()>,
{
    let started = Instant::now();
    let mut total = CalyxKvSweep::default();
    let mut interval_families = 0_usize;
    for cf_name in cf::ALL_COLUMN_FAMILIES {
        let sweep = sweep_calyx_namespace_live_rows(vault, cf_name, site, |key, payload| {
            visit(cf_name, key, payload)
        })?;
        if !sweep.atomic() {
            interval_families = interval_families.saturating_add(1);
        }
        total.merge(sweep);
    }
    tracing::info!(
        code = "STORAGE_CALYX_INSPECT_SWEEP_DONE",
        site,
        column_families = cf::ALL_COLUMN_FAMILIES.len(),
        pages = total.pages,
        page_rows = CALYX_INSPECT_SWEEP_PAGE_ROWS,
        rows_visited = total.rows_visited,
        rows_examined = total.rows_examined,
        elapsed_ms = started.elapsed().as_millis(),
        snapshot_seq_first = total.snapshot_seq_first,
        snapshot_seq_last = total.snapshot_seq_last,
        interval_families,
        "swept every Synapse column family with bounded row-table read-guard holds"
    );
    Ok(())
}

fn read_all_rows_from_vault(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    read_all_rows_from_vault_filtered(vault, cf_name, false)
}

fn read_all_rows_from_vault_including_expired(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    read_all_rows_from_vault_filtered(vault, cf_name, true)
}

fn read_all_rows_from_vault_filtered(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    include_expired: bool,
) -> StorageResult<Vec<RawRow>> {
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    read_rows_from_vault_range_filtered(vault, cf_name, &range, include_expired)
}

/// `site` label for the ordinary ordered-range read (`scan_cf`,
/// `scan_cf_prefix*`, `scan_cf_from`, and every read-only namespace scan).
///
/// Deliberately not a new *guard census* name — the row-guard sites stay
/// `scan_cf_range_page_latest` so windows collected before and after this change
/// stay directly comparable, which is the same reasoning `RowGuardSite::as_str`
/// and `CALYX_GC_RETENTION_SWEEP_SITE` already document. This label only
/// attributes `STORAGE_CALYX_BOUNDED_SWEEP` lines to the tool read rather than
/// to a maintenance tick.
const CALYX_ORDERED_RANGE_READ_SITE: &str = "calyx_ordered_range_read";

/// Materialises an ordered logical range with **bounded** row-table read-guard
/// holds (#2060).
///
/// # What changed and why the answer is unchanged
///
/// This used to be one `scan_kv_range_latest` per call — a single hold of the
/// shared MVCC row-table read guard across the merge and materialisation of a
/// whole namespace. Measured on the live daemon it was the last remaining
/// contributor to `scan_cf_range_latest`'s over-budget holds after #2058/#2041
/// and 682e6fdb: 25-30 ms per hold at very high rate under ordinary MCP tool
/// load, with a 224 ms worst hold, and every vault commit waits behind it.
///
/// It now folds through [`sweep_calyx_range_rows`] — the same pinned persistent
/// stream the GC retention sweep and the panel-coverage census use — releasing
/// each 256-row handoff without reconstructing the immutable merge. The decode,
/// TTL rule, include-expired switch and ordering assertion are unchanged.
///
/// Three properties of the row table make the paged fold **equal** to the held
/// one rather than an approximation of it (the argument established by #2058 and
/// restated for the census in #2060):
///
/// 1. Row-table entries are only ever created — snapshot GC trims version chains
///    in place — so the key order a cursor walks is append-only and stable and a
///    cursor can neither skip nor double-visit a key.
/// 2. A commit concurrent with the fold allocates a sequence strictly above the
///    one registered snapshot, so it is excluded from the complete answer.
/// 3. A concurrent reclaim keeps each chain's newest version at or below the
///    safe point, and that point is clamped to the oldest pinned sequence across
///    live leases, so the version this fold reads is never the one dropped. The
///    lease is re-checked while streaming, so a fold that outlived its pin fails
///    closed. The returned first/last sequence is also rejected unless atomic.
fn read_rows_from_vault_range_filtered(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    range: &KeyRange,
    include_expired: bool,
) -> StorageResult<Vec<RawRow>> {
    let mut decoded: Vec<RawRow> = Vec::new();
    sweep_calyx_range_rows(
        vault,
        cf_name,
        CALYX_ORDERED_RANGE_READ_SITE,
        range,
        |user_key, payload, expired| {
            if include_expired || !expired {
                decoded.push((user_key.to_vec(), payload.to_vec()));
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    Ok(decoded)
}

fn calyx_ordered_prefix_from_range(
    collection_id: u64,
    prefix: &[u8],
    start_key: &[u8],
) -> StorageResult<Option<KeyRange>> {
    if prefix.len() > CALYX_MAX_USER_KEY_BYTES || start_key.len() > CALYX_MAX_USER_KEY_BYTES {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name_for_calyx_collection_id(collection_id)
                .unwrap_or("<unknown>")
                .to_owned(),
            detail: format!(
                "ordered Calyx logical range exceeds the supported key maximum {CALYX_MAX_USER_KEY_BYTES}: prefix_len={} start_key_len={}",
                prefix.len(),
                start_key.len()
            ),
        });
    }
    let mut physical_prefix = calyx_namespace_prefix(collection_id);
    physical_prefix.extend_from_slice(prefix);
    let prefix_range = prefix_range(&physical_prefix);
    let start = if start_key > prefix {
        encode_calyx_key(collection_id, start_key).map_err(|detail| StorageError::ReadFailed {
            cf_name: cf_name_for_calyx_collection_id(collection_id)
                .unwrap_or("<unknown>")
                .to_owned(),
            detail,
        })?
    } else {
        physical_prefix
    };
    if prefix_range
        .end
        .as_deref()
        .is_some_and(|end| start.as_slice() >= end)
    {
        return Ok(None);
    }
    Ok(Some(KeyRange {
        start,
        end: prefix_range.end,
    }))
}

/// Hard ceiling on physical candidates one ordered logical page may walk while
/// skipping an unbroken run of TTL-expired rows.
///
/// A logical page is bounded by *live rows returned*, but the physical rows it
/// must step over to find them is a property of the corpus, not of the caller.
/// TTL-expired rows are filtered at read time and are only reclaimed later by
/// retention GC, so a column family whose writer stamps a per-session TTL
/// accumulates dead rows in contiguous key-order runs — every row of one
/// expired session sits adjacent to its siblings. Production measured 62,016
/// expired rows in `CF_AGENT_TRANSCRIPTS` and 17,674 in `CF_ACTION_LOG` against
/// 2,317 live, so a run far longer than any caller's page size is the normal
/// steady state, not an anomaly.
///
/// This bounds the resulting walk without capping it below what the corpus can
/// legitimately require. It is a safety net against a pathological vault, not a
/// tuning knob: exhausting it is reported as a hard error carrying the exact
/// resume position, never as a short read.
const CALYX_ORDERED_PAGE_MAX_SKIPPED_CANDIDATES: usize = 1_000_000;

/// Reads one ordered logical page, stepping over runs of TTL-expired rows.
///
/// The page is bounded by live rows, and it advances through dead candidates on
/// one pinned physical stream rather than reopening the range from a logical
/// key after each all-expired candidate page. That distinction is the whole
/// correctness and complexity argument. A logical cursor can only name a row
/// the caller was handed, while an expired row is intentionally not handed out.
/// The persistent immutable merge cursor can advance across that row anyway,
/// so forward progress no longer depends on finding something to return and a
/// long expired run remains one linear pass over its physical sources.
///
/// This first failed with `CALYX_ORDERED_KEY_PAGE_NO_LOGICAL_PROGRESS`. An
/// opaque resume cursor fixed correctness but still reopened and re-sought the
/// same KV SST hundreds of times for a 62,000-row expired run, sustaining more
/// than 1.5 GiB/s of reads and a full CPU core. The retained stream fixes that
/// physical amplification rather than hiding it behind a larger retry budget.
fn read_ordered_rows_from_vault_page(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    start_key: &[u8],
    max_rows: usize,
) -> StorageResult<ScanWindow> {
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let namespace_range = prefix_range(&calyx_namespace_prefix(collection_id));
    let range = KeyRange {
        start: encode_calyx_key_for_read(cf_name, collection_id, start_key)?,
        end: namespace_range.end,
    };
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;

    let mut decoded: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut candidates_examined = 0usize;
    let mut expired_skipped = 0usize;
    let candidate_page_rows = max_rows.max(SYNAPSE_CALYX_CF_WALK_PAGE_ROWS);
    let walk = vault
        .walk_cf_range_latest_snapshot(
            ColumnFamily::Kv,
            &range,
            candidate_page_rows,
            |physical_key, value| {
                candidates_examined = candidates_examined.saturating_add(1);
                if candidates_examined > CALYX_ORDERED_PAGE_MAX_SKIPPED_CANDIDATES {
                    return Err(SynapseCalyxError::new(
                        "CALYX_ORDERED_KEY_PAGE_EXPIRED_RUN_EXCEEDS_BUDGET",
                        format!(
                            "walked more than {CALYX_ORDERED_PAGE_MAX_SKIPPED_CANDIDATES} consecutive physical candidates without filling the {max_rows}-row logical {cf_name} page; current_physical_key_sha256={}",
                            sha256_hex(physical_key)
                        ),
                        "run retention GC for the named CF to reclaim the expired run, then retry",
                    ));
                }
                let user_key = decode_calyx_user_key_for_read(
                    cf_name,
                    collection_id,
                    physical_key,
                )
                .map_err(|error| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_ORDERED_KEY_DECODE_FAILED",
                        error.to_string(),
                        "repair the named physical namespace key before retrying the ordered page",
                    )
                })?;
                let envelope = decode_calyx_value_raw(value).map_err(|detail| {
                    SynapseCalyxError::new(
                        "SYNAPSE_CALYX_ORDERED_VALUE_DECODE_FAILED",
                        format!(
                            "decode candidate-bounded ordered Calyx logical page value: key_sha256={} detail={detail}",
                            sha256_hex(&user_key)
                        ),
                        "repair the named retention envelope before retrying the ordered page",
                    )
                })?;
                if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
                    expired_skipped = expired_skipped.saturating_add(1);
                } else {
                    decoded.push((user_key, envelope.payload.to_vec()));
                }
                Ok(if decoded.len() >= max_rows {
                    SynapseCalyxWalkStep::Stop
                } else {
                    SynapseCalyxWalkStep::Continue
                })
            },
        )
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "stream candidate-bounded ordered Calyx logical page",
                &source,
            )
        })?;
    let more = walk.stopped_early;

    if !decoded
        .windows(2)
        .all(|pair| pair[0].0.as_slice() < pair[1].0.as_slice())
    {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "CALYX_ORDERED_KEY_PAGE_OUT_OF_ORDER: candidate-bounded page decoded into non-increasing logical keys; remediation=inspect namespace-one physical keys"
                .to_owned(),
        });
    }

    if expired_skipped > 0 {
        tracing::warn!(
            code = "STORAGE_CALYX_ORDERED_PAGE_SKIPPED_EXPIRED_RUN",
            cf = cf_name,
            physical_pages = walk.pages,
            candidates_examined,
            expired_skipped,
            live_rows = decoded.len(),
            "ordered Calyx logical page stepped over a run of TTL-expired rows on the physical cursor; retention GC has not reclaimed them"
        );
    }

    Ok((decoded, more))
}

fn fixed_width_user_key_len(cf_name: &str) -> Option<usize> {
    match cf_name {
        cf::CF_TIMELINE => Some(crate::timeline::TIMELINE_KEY_LEN),
        cf::CF_EPISODES => Some(crate::episodes::EPISODE_KEY_LEN),
        _ => None,
    }
}

fn read_fixed_width_rows_from_vault_range(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    start_key: &[u8],
    end_key: &[u8],
    max_rows: usize,
) -> StorageResult<ScanWindow> {
    let Some(key_len) = fixed_width_user_key_len(cf_name) else {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail:
                "bounded Calyx range scan is only available for fixed-width key column families"
                    .to_owned(),
        });
    };
    if start_key.len() != key_len || end_key.len() != key_len {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "bounded Calyx range scan for {cf_name} requires {key_len}-byte keys; got start={} end={}",
                start_key.len(),
                end_key.len()
            ),
        });
    }
    if start_key >= end_key {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "bounded Calyx range scan requires start_key < end_key".to_owned(),
        });
    }

    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = KeyRange {
        start: encode_calyx_key_for_read(cf_name, collection_id, start_key)?,
        end: Some(encode_calyx_key_for_read(cf_name, collection_id, end_key)?),
    };
    let mut decoded = Vec::with_capacity(max_rows.min(CALYX_INSPECT_SWEEP_PAGE_ROWS));
    let mut more = false;
    let sweep = sweep_calyx_range_rows(
        vault,
        cf_name,
        "calyx_fixed_width_range_read",
        &range,
        |user_key, payload, expired| {
            if expired {
                return Ok(ControlFlow::Continue(()));
            }
            if decoded.len() == max_rows {
                more = true;
                return Ok(ControlFlow::Break(()));
            }
            decoded.push((user_key.to_vec(), payload.to_vec()));
            Ok(ControlFlow::Continue(()))
        },
    )?;
    if !sweep.atomic() {
        tracing::debug!(
            code = "STORAGE_CALYX_FIXED_WIDTH_RANGE_READ_INTERVAL",
            cf = cf_name,
            site = "calyx_fixed_width_range_read",
            pages = sweep.pages,
            rows_visited = sweep.rows_visited,
            snapshot_seq_first = sweep.snapshot_seq_first,
            snapshot_seq_last = sweep.snapshot_seq_last,
            stopped_early = sweep.stopped_early,
            max_rows,
            more,
            "fixed-width Calyx range read spanned more than one committed sequence; rows are individually current and the set is a bounded interval"
        );
    }
    Ok((decoded, more))
}

fn validate_physical_page_request(
    cf_name: &str,
    after_physical: Option<&[u8]>,
    max_rows: usize,
) -> StorageResult<()> {
    if max_rows > LATEST_CF_RANGE_PAGE_MAX_ROWS {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "candidate-bounded physical Calyx page max_rows {max_rows} exceeds the hard maximum {LATEST_CF_RANGE_PAGE_MAX_ROWS}"
            ),
        });
    }
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    if let Some(cursor) = after_physical {
        decode_calyx_user_key_for_read(cf_name, collection_id, cursor).map_err(|error| {
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "candidate-bounded physical Calyx page rejected an opaque cursor outside the exact {cf_name} namespace: cursor_hex={} detail={error}",
                    hex_prefix_for_log(cursor)
                ),
            }
        })?;
    }
    Ok(())
}

fn pin_coherent_scan(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    scope: CoherentScanScope,
    max_age_ms: u64,
) -> StorageResult<CoherentScanLease> {
    if max_age_ms == 0 || max_age_ms > crate::COHERENT_SCAN_MAX_AGE_MS {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "COHERENT_SCAN_LEASE_AGE_INVALID: requested max_age_ms={max_age_ms}; permitted=1..={}; remediation=use a bounded scan or an explicit delta-first rebase",
                crate::COHERENT_SCAN_MAX_AGE_MS
            ),
        });
    }
    let snapshot = vault
        .pin_reader(Freshness::FreshDerived, max_age_ms)
        .map_err(|source| calyx_read_failed(cf_name, "pin coherent Calyx scan", &source))?;
    let read_at_unix_ms = match vault.clock_now_ms() {
        Ok(now) => now,
        Err(source) => {
            let _released = vault.release_reader(snapshot.lease().id());
            return Err(calyx_read_failed(
                cf_name,
                "read Calyx clock for coherent scan",
                &source,
            ));
        }
    };
    let lease = snapshot.lease();
    tracing::debug!(
        code = "STORAGE_COHERENT_SCAN_PINNED",
        lease_id = lease.id(),
        snapshot_seq = snapshot.seq(),
        issued_at_unix_ms = lease.issued_at(),
        expires_at_unix_ms = lease.expires_at(),
        read_at_unix_ms,
        cf_name,
        "pinned bounded coherent scan lease"
    );
    Ok(CoherentScanLease {
        lease_id: lease.id(),
        snapshot_seq: snapshot.seq(),
        issued_at_unix_ms: lease.issued_at(),
        expires_at_unix_ms: lease.expires_at(),
        read_at_unix_ms,
        cf_name: cf_name.to_owned(),
        snapshot,
        scope,
        next_after: None,
        started: false,
        completed: false,
        released: false,
    })
}

fn coherent_scan_contract_error(lease: &CoherentScanLease, detail: &str) -> StorageError {
    StorageError::ReadFailed {
        cf_name: lease.cf_name.clone(),
        detail: format!(
            "{detail}; lease_id={} snapshot_seq={} issued_at_unix_ms={} expires_at_unix_ms={} started={} completed={} released={}",
            lease.lease_id,
            lease.snapshot_seq,
            lease.issued_at_unix_ms,
            lease.expires_at_unix_ms,
            lease.started,
            lease.completed,
            lease.released
        ),
    }
}

fn ensure_coherent_scan_ready(
    lease: &CoherentScanLease,
    expected_scope: &str,
) -> StorageResult<()> {
    if lease.released {
        return Err(coherent_scan_contract_error(
            lease,
            "COHERENT_SCAN_RELEASED: continuation attempted after explicit release",
        ));
    }
    if lease.completed {
        return Err(coherent_scan_contract_error(
            lease,
            "COHERENT_SCAN_COMPLETE: continuation attempted after the terminal page",
        ));
    }
    tracing::trace!(
        lease_id = lease.lease_id,
        snapshot_seq = lease.snapshot_seq,
        cf_name = %lease.cf_name,
        expected_scope,
        next_after_len = lease.next_after.as_ref().map_or(0, Vec::len),
        "validated coherent scan continuation state"
    );
    Ok(())
}

fn advance_coherent_scan(
    lease: &mut CoherentScanLease,
    resume_after: Option<&[u8]>,
    more: bool,
) -> StorageResult<()> {
    if more && resume_after.is_none() {
        return Err(coherent_scan_contract_error(
            lease,
            "COHERENT_SCAN_CURSOR_MISSING: page reported more rows without an exclusive cursor",
        ));
    }
    if let Some(resume) = resume_after
        && lease
            .next_after
            .as_deref()
            .is_some_and(|previous| resume <= previous)
    {
        return Err(coherent_scan_contract_error(
            lease,
            "COHERENT_SCAN_CURSOR_NON_PROGRESSING: returned cursor did not advance strictly",
        ));
    }
    lease.started = true;
    lease.completed = !more;
    lease.next_after = more.then(|| resume_after.unwrap_or_default().to_vec());
    Ok(())
}

fn read_physical_page_from_vault_snapshot(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    snapshot: calyx_aster::mvcc::Snapshot,
    after_physical: Option<&[u8]>,
    max_rows: usize,
    read_at_unix_ms: u64,
) -> StorageResult<PhysicalScanPage> {
    validate_physical_page_request(cf_name, after_physical, max_rows)?;
    if max_rows == 0 {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "COHERENT_SCAN_PAGE_SIZE_ZERO: coherent continuation requires max_rows > 0"
                .to_owned(),
        });
    }
    let candidate_budget = max_rows
        .checked_add(1)
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "coherent physical page max_rows cannot be usize::MAX".to_owned(),
        })?;
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let page = vault
        .scan_cf_range_page_snapshot(snapshot, ColumnFamily::Kv, &range, after_physical, max_rows)
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "scan pinned candidate-bounded physical Calyx KV namespace page",
                &source,
            )
        })?;
    validate_physical_page_shape(
        cf_name,
        collection_id,
        after_physical,
        max_rows,
        candidate_budget,
        &page,
    )?;
    let (rows, expired_rows_skipped) = decode_physical_page_rows(
        cf_name,
        collection_id,
        after_physical,
        read_at_unix_ms,
        &page,
    )?;
    Ok(PhysicalScanPage {
        rows,
        resume_after_physical: page.resume_after,
        more: page.more,
        snapshot_seq: Some(page.snapshot_seq),
        candidate_rows_examined: page.examined_rows,
        expired_rows_skipped,
    })
}

fn read_physical_page_from_vault(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    after_physical: Option<&[u8]>,
    max_rows: usize,
) -> StorageResult<PhysicalScanPage> {
    validate_physical_page_request(cf_name, after_physical, max_rows)?;
    if max_rows == 0 {
        return Ok(PhysicalScanPage::empty());
    }
    let candidate_budget = max_rows.checked_add(1).ok_or_else(|| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail: "candidate-bounded physical Calyx page max_rows cannot be usize::MAX because exact continuation requires one lookahead candidate"
            .to_owned(),
    })?;
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let page = vault
        .scan_kv_range_page_latest(&range, after_physical, max_rows)
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "scan candidate-bounded physical Calyx KV namespace page",
                &source,
            )
        })?;
    validate_physical_page_shape(
        cf_name,
        collection_id,
        after_physical,
        max_rows,
        candidate_budget,
        &page,
    )?;
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    let (rows, expired_rows_skipped) =
        decode_physical_page_rows(cf_name, collection_id, after_physical, now_ms, &page)?;
    Ok(PhysicalScanPage {
        rows,
        resume_after_physical: page.resume_after,
        more: page.more,
        snapshot_seq: Some(page.snapshot_seq),
        candidate_rows_examined: page.examined_rows,
        expired_rows_skipped,
    })
}

fn read_physical_prefix_page_from_vault(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    prefix: &[u8],
    after_physical: Option<&[u8]>,
    max_rows: usize,
) -> StorageResult<PhysicalScanPage> {
    validate_physical_page_request(cf_name, after_physical, max_rows)?;
    if max_rows == 0 {
        return Ok(PhysicalScanPage::empty());
    }
    let candidate_budget = max_rows
        .checked_add(1)
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "candidate-bounded prefix page max_rows cannot be usize::MAX".to_owned(),
        })?;
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = calyx_ordered_prefix_from_range(collection_id, prefix, prefix)?.ok_or_else(|| {
        StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "CALYX_PREFIX_RANGE_EMPTY: declared MCP-usage prefix produced no ordered key range; remediation=inspect the physical key codec"
                .to_owned(),
        }
    })?;
    if let Some(cursor) = after_physical {
        let logical = decode_calyx_user_key_for_read(cf_name, collection_id, cursor)?;
        if !logical.starts_with(prefix) {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "CALYX_PREFIX_CURSOR_OUTSIDE_SCOPE: opaque cursor is outside the declared prefix; cursor_hex={}; remediation=restart this prefix sweep without a cursor",
                    hex_prefix_for_log(cursor)
                ),
            });
        }
    }
    let page = vault
        .scan_kv_range_page_latest(&range, after_physical, max_rows)
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "scan candidate-bounded physical Calyx prefix page",
                &source,
            )
        })?;
    validate_physical_page_shape(
        cf_name,
        collection_id,
        after_physical,
        max_rows,
        candidate_budget,
        &page,
    )?;
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    let (rows, expired_rows_skipped) =
        decode_physical_page_rows(cf_name, collection_id, after_physical, now_ms, &page)?;
    if rows.iter().any(|(key, _)| !key.starts_with(prefix)) {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "CALYX_PREFIX_PAGE_ESCAPED_SCOPE: physical range returned a row outside its declared prefix; remediation=inspect the ordered prefix range codec"
                .to_owned(),
        });
    }
    Ok(PhysicalScanPage {
        rows,
        resume_after_physical: page.resume_after,
        more: page.more,
        snapshot_seq: Some(page.snapshot_seq),
        candidate_rows_examined: page.examined_rows,
        expired_rows_skipped,
    })
}

fn validate_physical_page_shape(
    cf_name: &str,
    collection_id: u64,
    after_physical: Option<&[u8]>,
    max_rows: usize,
    candidate_budget: usize,
    page: &SynapseCalyxCfRangePage,
) -> StorageResult<()> {
    let resume_present = page.resume_after.is_some();
    let expected_resume = page.examined_rows != 0;
    let expected_more = page.examined_rows > max_rows;
    if page.examined_rows > candidate_budget
        || page.rows.len() > max_rows
        || page.rows.len() > page.examined_rows
        || resume_present != expected_resume
        || page.more != expected_more
    {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "candidate-bounded physical Calyx page violated its shape/budget contract: snapshot_seq={} examined={} candidate_budget={candidate_budget} output_rows={} output_budget={max_rows} more={} expected_more={expected_more} resume_present={resume_present} expected_resume={expected_resume}",
                page.snapshot_seq,
                page.examined_rows,
                page.rows.len(),
                page.more
            ),
        });
    }
    if let Some(resume) = page.resume_after.as_deref() {
        decode_calyx_user_key_for_read(cf_name, collection_id, resume).map_err(|error| {
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "candidate-bounded physical Calyx page returned an invalid namespace cursor: snapshot_seq={} cursor_hex={} detail={error}",
                    page.snapshot_seq,
                    hex_prefix_for_log(resume)
                ),
            }
        })?;
        if after_physical.is_some_and(|after| resume <= after) {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "candidate-bounded physical Calyx page returned a non-progressing cursor: snapshot_seq={} previous_cursor_hex={} returned_cursor_hex={}",
                    page.snapshot_seq,
                    after_physical.map_or_else(|| "none".to_owned(), hex_prefix_for_log),
                    hex_prefix_for_log(resume)
                ),
            });
        }
    }
    Ok(())
}

fn decode_physical_page_rows(
    cf_name: &str,
    collection_id: u64,
    after_physical: Option<&[u8]>,
    now_ms: u64,
    page: &SynapseCalyxCfRangePage,
) -> StorageResult<(Vec<RawRow>, usize)> {
    let mut rows = Vec::with_capacity(page.rows.len());
    let mut expired_rows_skipped = 0_usize;
    let mut previous_key = after_physical;
    for (physical_key, value) in &page.rows {
        if previous_key.is_some_and(|previous| physical_key.as_slice() <= previous)
            || page
                .resume_after
                .as_deref()
                .is_none_or(|resume| physical_key.as_slice() > resume)
        {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "candidate-bounded physical Calyx page returned an unordered or out-of-cursor row: snapshot_seq={} previous_key_hex={} row_key_hex={} resume_after_hex={}",
                    page.snapshot_seq,
                    previous_key.map_or_else(|| "none".to_owned(), hex_prefix_for_log),
                    hex_prefix_for_log(physical_key),
                    page.resume_after
                        .as_deref()
                        .map_or_else(|| "none".to_owned(), hex_prefix_for_log)
                ),
            });
        }
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, physical_key)?;
        let envelope = decode_calyx_value_raw(value).map_err(|detail| {
            tracing::error!(
                code = error_codes::STORAGE_READ_FAILED,
                cf = cf_name,
                physical_key_hex = %hex_prefix_for_log(physical_key),
                detail,
                "Calyx storage backend rejected a malformed retention envelope during a candidate-bounded physical page"
            );
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "candidate-bounded physical Calyx page found a malformed retention envelope at physical_key_hex={}: {detail}",
                    hex_prefix_for_log(physical_key)
                ),
            }
        })?;
        if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            expired_rows_skipped = expired_rows_skipped.saturating_add(1);
        } else {
            rows.push((user_key, envelope.payload.to_vec()));
        }
        previous_key = Some(physical_key);
    }
    Ok((rows, expired_rows_skipped))
}

fn validate_fixed_width_page_request(
    cf_name: &str,
    start_key: &[u8],
    end_key: &[u8],
    after_key: Option<&[u8]>,
    key_len: usize,
    max_rows: usize,
) -> StorageResult<()> {
    if key_len == 0
        || start_key.len() != key_len
        || end_key.len() != key_len
        || after_key.is_some_and(|key| key.len() != key_len)
    {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "candidate-bounded Calyx range page for {cf_name} requires one non-zero {key_len}-byte key width; got start={} end={} after={}",
                start_key.len(),
                end_key.len(),
                after_key.map_or_else(|| "none".to_owned(), |key| key.len().to_string())
            ),
        });
    }
    if key_len > CALYX_MAX_USER_KEY_BYTES {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "candidate-bounded Calyx range page key width {key_len} exceeds the physical envelope maximum {CALYX_MAX_USER_KEY_BYTES}"
            ),
        });
    }
    if max_rows > LATEST_CF_RANGE_PAGE_MAX_ROWS {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "candidate-bounded Calyx range page max_rows {max_rows} exceeds the hard maximum {LATEST_CF_RANGE_PAGE_MAX_ROWS}"
            ),
        });
    }
    if start_key >= end_key {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "candidate-bounded Calyx range page requires start_key < end_key".to_owned(),
        });
    }
    if after_key.is_some_and(|key| key < start_key || key >= end_key) {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail:
                "candidate-bounded Calyx range page requires after_key inside [start_key, end_key)"
                    .to_owned(),
        });
    }
    Ok(())
}

fn read_fixed_width_page_from_vault_range(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    start_key: &[u8],
    end_key: &[u8],
    after_key: Option<&[u8]>,
    key_len: usize,
    max_rows: usize,
) -> StorageResult<FixedWidthScanPage> {
    validate_fixed_width_page_request(cf_name, start_key, end_key, after_key, key_len, max_rows)?;
    if max_rows == 0 {
        return Ok(FixedWidthScanPage::empty());
    }
    let candidate_budget = max_rows.checked_add(1).ok_or_else(|| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail:
            "candidate-bounded Calyx range page max_rows cannot be usize::MAX because exact continuation requires one lookahead candidate"
                .to_owned(),
    })?;

    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = KeyRange {
        start: encode_calyx_key_for_read(cf_name, collection_id, start_key)?,
        end: Some(encode_calyx_key_for_read(cf_name, collection_id, end_key)?),
    };
    let physical_after = after_key
        .map(|key| encode_calyx_key_for_read(cf_name, collection_id, key))
        .transpose()?;
    let page = vault
        .scan_kv_range_page_latest(&range, physical_after.as_deref(), max_rows)
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "scan candidate-bounded Calyx KV fixed-width range page",
                &source,
            )
        })?;
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    decode_fixed_width_page(
        FixedWidthPageDecodeRequest {
            cf_name,
            collection_id,
            start_key,
            end_key,
            after_key,
            key_len,
            max_rows,
            candidate_budget,
            now_ms,
        },
        page,
    )
}

#[allow(clippy::too_many_arguments)]
fn read_fixed_width_page_from_vault_range_snapshot(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    snapshot: calyx_aster::mvcc::Snapshot,
    start_key: &[u8],
    end_key: &[u8],
    after_key: Option<&[u8]>,
    key_len: usize,
    max_rows: usize,
    read_at_unix_ms: u64,
) -> StorageResult<FixedWidthScanPage> {
    validate_fixed_width_page_request(cf_name, start_key, end_key, after_key, key_len, max_rows)?;
    if max_rows == 0 {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "COHERENT_SCAN_PAGE_SIZE_ZERO: coherent continuation requires max_rows > 0"
                .to_owned(),
        });
    }
    let candidate_budget = max_rows
        .checked_add(1)
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "coherent fixed-width page max_rows cannot be usize::MAX".to_owned(),
        })?;
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = KeyRange {
        start: encode_calyx_key_for_read(cf_name, collection_id, start_key)?,
        end: Some(encode_calyx_key_for_read(cf_name, collection_id, end_key)?),
    };
    let physical_after = after_key
        .map(|key| encode_calyx_key_for_read(cf_name, collection_id, key))
        .transpose()?;
    let page = vault
        .scan_cf_range_page_snapshot(
            snapshot,
            ColumnFamily::Kv,
            &range,
            physical_after.as_deref(),
            max_rows,
        )
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "scan pinned Calyx KV fixed-width range page",
                &source,
            )
        })?;
    decode_fixed_width_page(
        FixedWidthPageDecodeRequest {
            cf_name,
            collection_id,
            start_key,
            end_key,
            after_key,
            key_len,
            max_rows,
            candidate_budget,
            now_ms: read_at_unix_ms,
        },
        page,
    )
}

#[derive(Clone, Copy)]
struct FixedWidthPageDecodeRequest<'a> {
    cf_name: &'a str,
    collection_id: u64,
    start_key: &'a [u8],
    end_key: &'a [u8],
    after_key: Option<&'a [u8]>,
    key_len: usize,
    max_rows: usize,
    candidate_budget: usize,
    now_ms: u64,
}

fn decode_fixed_width_page(
    request: FixedWidthPageDecodeRequest<'_>,
    page: SynapseCalyxCfRangePage,
) -> StorageResult<FixedWidthScanPage> {
    let FixedWidthPageDecodeRequest {
        cf_name,
        collection_id,
        start_key,
        end_key,
        after_key,
        key_len,
        max_rows,
        candidate_budget,
        now_ms,
    } = request;
    if page.examined_rows > candidate_budget || page.rows.len() > max_rows {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "Calyx fixed-width page exceeded candidate/output budget: examined={} candidate_budget={candidate_budget} output_rows={} output_budget={max_rows}",
                page.examined_rows,
                page.rows.len()
            ),
        });
    }
    if page.more && page.resume_after.is_none() {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "Calyx fixed-width page reported more rows without a resume cursor".to_owned(),
        });
    }
    let mut rows = Vec::with_capacity(page.rows.len());
    let mut expired_rows_skipped = 0;
    for (key, value) in page.rows {
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, &key)?;
        if user_key.len() != key_len
            || user_key.as_slice() < start_key
            || user_key.as_slice() >= end_key
            || after_key.is_some_and(|after| user_key.as_slice() <= after)
        {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "Calyx fixed-width page returned an out-of-contract logical key: expected width={key_len} range=[{}, {}) after={} actual_width={}",
                    hex_prefix_for_log(start_key),
                    hex_prefix_for_log(end_key),
                    after_key.map_or_else(|| "none".to_owned(), hex_prefix_for_log),
                    user_key.len()
                ),
            });
        }
        let envelope = decode_calyx_value_raw(&value).map_err(|detail| {
            tracing::error!(
                code = error_codes::STORAGE_READ_FAILED,
                cf = cf_name,
                detail,
                "Calyx storage backend rejected malformed KV retention envelope during candidate-bounded range page"
            );
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail,
            }
        })?;
        if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            expired_rows_skipped += 1;
        } else {
            rows.push((user_key, envelope.payload.to_vec()));
        }
    }
    let resume_after = page
        .resume_after
        .as_deref()
        .map(|key| decode_calyx_user_key_for_read(cf_name, collection_id, key))
        .transpose()?;
    if resume_after.as_ref().is_some_and(|key| {
        key.len() != key_len
            || key.as_slice() < start_key
            || key.as_slice() >= end_key
            || after_key.is_some_and(|after| key.as_slice() <= after)
    }) {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "Calyx fixed-width page returned a non-progressing or out-of-range cursor"
                .to_owned(),
        });
    }
    Ok(FixedWidthScanPage {
        rows,
        resume_after,
        more: page.more,
        snapshot_seq: Some(page.snapshot_seq),
        candidate_rows_examined: page.examined_rows,
        expired_rows_skipped,
    })
}

fn hex_prefix_for_log(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(27);
    for byte in bytes.iter().take(12) {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    if bytes.len() > 12 {
        value.push_str("...");
    }
    value
}

fn build_episode_constellation_batch(
    vault: &SynapseCalyxVault,
    rows: &[(Vec<u8>, Vec<u8>, EpisodeRecord)],
) -> StorageResult<ConstellationBatch> {
    let vault_id = vault.vault_id_value();
    let created_at_ms = calyx_clock_now_for_write(vault, cf::CF_EPISODES)?;
    let next_ledger_seq = vault.latest_seq().saturating_add(1);
    let mut constellations = Vec::with_capacity(rows.len());
    let mut pending_reports = Vec::with_capacity(rows.len());
    for (source_key, raw_bytes, record) in rows {
        let context = NativeConstellationContext {
            vault_id,
            cx_id: vault.cx_id_for_input(raw_bytes, SYN_EPISODE_PANEL_VERSION),
            created_at_ms,
            next_ledger_seq,
        };
        let constellation =
            constellations::build_episode_constellation(context, source_key, raw_bytes, record)?;
        pending_reports.push(PendingConstellationReport {
            source_key: source_key.clone(),
            raw_bytes: raw_bytes.clone(),
            slot_count: constellation.slots.len() as u64,
            scalar_count: constellation.scalars.len() as u64,
        });
        constellations.push(constellation);
    }
    Ok(ConstellationBatch {
        constellations,
        pending_reports,
    })
}

fn build_agent_event_constellation_batch(
    vault: &SynapseCalyxVault,
    rows: &[(Vec<u8>, Vec<u8>, AgentEventRecord)],
) -> StorageResult<ConstellationBatch> {
    let vault_id = vault.vault_id_value();
    let created_at_ms = calyx_clock_now_for_write(vault, cf::CF_AGENT_EVENTS)?;
    let next_ledger_seq = vault.latest_seq().saturating_add(1);
    let built = rows
        .par_iter()
        .map(|(source_key, raw_bytes, record)| {
            let context = NativeConstellationContext {
                vault_id,
                cx_id: vault.cx_id_for_input(raw_bytes, SYN_AGENT_EVENT_PANEL_VERSION),
                created_at_ms,
                next_ledger_seq,
            };
            let constellation = constellations::build_agent_event_constellation(
                context, source_key, raw_bytes, record,
            )?;
            let report = PendingConstellationReport {
                source_key: source_key.clone(),
                raw_bytes: raw_bytes.clone(),
                slot_count: constellation.slots.len() as u64,
                scalar_count: constellation.scalars.len() as u64,
            };
            Ok((constellation, report))
        })
        .collect::<Vec<StorageResult<_>>>();
    collect_constellation_batch(built)
}

fn build_agent_transcript_constellation_batch(
    vault: &SynapseCalyxVault,
    rows: &[(Vec<u8>, Vec<u8>, AgentTranscriptRecord)],
) -> StorageResult<ConstellationBatch> {
    let vault_id = vault.vault_id_value();
    let created_at_ms = calyx_clock_now_for_write(vault, cf::CF_AGENT_TRANSCRIPTS)?;
    let next_ledger_seq = vault.latest_seq().saturating_add(1);
    let built = rows
        .par_iter()
        .map(|(source_key, raw_bytes, record)| {
            let context = NativeConstellationContext {
                vault_id,
                cx_id: vault.cx_id_for_input(raw_bytes, SYN_AGENT_TRANSCRIPT_PANEL_VERSION),
                created_at_ms,
                next_ledger_seq,
            };
            let constellation = constellations::build_agent_transcript_constellation(
                context, source_key, raw_bytes, record,
            )?;
            let report = PendingConstellationReport {
                source_key: source_key.clone(),
                raw_bytes: raw_bytes.clone(),
                slot_count: constellation.slots.len() as u64,
                scalar_count: constellation.scalars.len() as u64,
            };
            Ok((constellation, report))
        })
        .collect::<Vec<StorageResult<_>>>();
    collect_constellation_batch(built)
}

fn collect_constellation_batch(
    built: Vec<StorageResult<(Constellation, PendingConstellationReport)>>,
) -> StorageResult<ConstellationBatch> {
    let built = built.into_iter().collect::<StorageResult<Vec<_>>>()?;
    let mut constellations = Vec::with_capacity(built.len());
    let mut pending_reports = Vec::with_capacity(built.len());
    for (constellation, report) in built {
        constellations.push(constellation);
        pending_reports.push(report);
    }
    Ok(ConstellationBatch {
        constellations,
        pending_reports,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the emitted batch evidence names the complete frozen panel identity and source surface"
)]
fn emit_constellation_batch_result(
    result: StorageResult<Vec<ConstellationPutReport>>,
    input_rows: usize,
    panel_name: &'static str,
    panel_version: u32,
    source_cf: &'static str,
    success_code: &'static str,
    started: Instant,
) -> StorageResult<Vec<ConstellationPutReport>> {
    match result {
        Ok(reports) => {
            for report in &reports {
                constellations::emit_success_metric(report);
            }
            let inserted = reports.iter().filter(|report| report.inserted()).count();
            let deduped = reports.iter().filter(|report| report.deduped()).count();
            tracing::debug!(
                code = success_code,
                panel_name,
                panel_version,
                source_cf,
                input_rows,
                inserted,
                deduped,
                duration_us = constellations::duration_us(started.elapsed()),
                "rows measured into native Calyx constellations as one durable batch"
            );
            Ok(reports)
        }
        Err(error) => {
            constellations::emit_error_metric(
                panel_name,
                source_cf,
                error.code(),
                started.elapsed(),
            );
            Err(error)
        }
    }
}

fn constellation_batch_reports(
    pending: Vec<PendingConstellationReport>,
    readbacks: Vec<SynapseCalyxObservationPutReadback>,
    panel_name: &'static str,
    panel_version: u32,
    source_cf: &'static str,
    duration_us: u64,
) -> StorageResult<Vec<ConstellationPutReport>> {
    if readbacks.len() != pending.len() {
        return Err(calyx_write_failed_detail(
            "calyx_constellation",
            format!(
                "{panel_name} constellation batch returned {} readbacks for {} inputs",
                readbacks.len(),
                pending.len()
            ),
        ));
    }
    Ok(pending
        .into_iter()
        .zip(readbacks)
        .map(|(input, readback)| {
            constellation_report(ConstellationReportInput {
                panel_name,
                panel_version,
                source_cf,
                source_key: &input.source_key,
                raw_bytes: &input.raw_bytes,
                readback,
                slot_count: input.slot_count,
                scalar_count: input.scalar_count,
                duration_us,
            })
        })
        .collect())
}

fn commit_calyx_rows_to_vault(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    rows: Vec<SynapseCalyxCfWrite>,
) -> StorageResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let plan = plan_calyx_write_batches(cf_name, rows)?;
    let chunk_count = plan.chunks.len();
    let total_rows = plan.total_rows;
    let total_payload_bytes = plan.total_payload_bytes;
    let largest_row_payload_bytes = plan.largest_row_payload_bytes;
    for (chunk_index, chunk) in plan.chunks.into_iter().enumerate() {
        let chunk_rows = chunk.rows.len();
        let chunk_payload_bytes = chunk.payload_bytes;
        vault.write_cf_batch(chunk.rows).map_err(|source| {
            calyx_write_failed(
                cf_name,
                &format!(
                    "write Calyx CF batch chunk {}/{} rows={} estimated_payload_bytes={} max_payload_bytes={} wal_max_record_bytes={}",
                    chunk_index + 1,
                    chunk_count,
                    chunk_rows,
                    chunk_payload_bytes,
                    CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES,
                    wal::MAX_RECORD_BYTES
                ),
                &source,
            )
        })?;
        if chunk_count > 1 {
            tracing::info!(
                code = "STORAGE_CALYX_WRITE_BATCH_CHUNK_COMMITTED",
                cf = cf_name,
                chunk_index = chunk_index + 1,
                chunk_count,
                chunk_rows,
                chunk_payload_bytes,
                max_payload_bytes = CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES,
                wal_max_record_bytes = wal::MAX_RECORD_BYTES,
                "committed bounded Calyx CF write-batch chunk"
            );
        }
    }
    if chunk_count > 1 {
        tracing::info!(
            code = "STORAGE_CALYX_WRITE_BATCH_CHUNKED",
            cf = cf_name,
            chunk_count,
            total_rows,
            total_payload_bytes,
            largest_row_payload_bytes,
            max_payload_bytes = CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES,
            wal_max_record_bytes = wal::MAX_RECORD_BYTES,
            "Calyx CF write batch was split below the WAL record ceiling"
        );
    }
    // Every chunk has already crossed its own fsynced WAL boundary. Do not run
    // a post-commit maintenance flush in the logical write path: if it failed,
    // callers would receive a retryable-looking error after durable mutation.
    // Explicit storage flush/checkpoint operations retain their own observable
    // error contract.
    Ok(())
}

fn commit_calyx_rows_atomically_to_vault(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    rows: Vec<SynapseCalyxCfWrite>,
) -> StorageResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let plan = plan_calyx_write_batches(cf_name, rows)?;
    let mut chunks = plan.chunks.into_iter();
    let chunk = chunks.next().ok_or_else(|| {
        calyx_write_failed_detail(
            cf_name,
            "atomic Calyx batch planner returned no rows for non-empty input".to_owned(),
        )
    })?;
    if chunks.next().is_some() {
        return Err(revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE,
            format!(
                "atomic Calyx batch exceeds one WAL record: total_rows={} total_payload_bytes={} max_payload_bytes={}; split was refused because the API promises cross-CF atomicity",
                plan.total_rows, plan.total_payload_bytes, CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES
            ),
        ));
    }
    vault.write_cf_batch(chunk.rows).map_err(|source| {
        calyx_write_failed(
            cf_name,
            &format!(
                "write one atomic Calyx batch rows={} estimated_payload_bytes={} max_payload_bytes={}",
                plan.total_rows, plan.total_payload_bytes, CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES
            ),
            &source,
        )
    })?;
    // `write_cf_batch` has already crossed Calyx's fsynced WAL boundary. A
    // subsequent memtable/SST flush is maintenance, not part of this logical
    // commit; surfacing its failure here would turn a known-applied operation
    // into an ordinary error and invite an unsafe retry.
    Ok(())
}

fn validate_cross_cf_revision_guarded_put(
    guards: &[CfRevisionGuard],
    batches: &[OwnedCfWriteBatch],
) -> StorageResult<()> {
    const CF_NAME: &str = "<multi-cf>";
    if guards.is_empty() {
        return Err(revision_guarded_mutation_failed(
            CF_NAME,
            STORAGE_REVISION_GUARD_INVALID,
            "cross-CF revision-guarded write requires at least one logical guard".to_owned(),
        ));
    }
    let mut mutation_counts = BTreeMap::<(&str, &[u8]), usize>::new();
    for (batch_index, (cf_name, rows)) in batches.iter().enumerate() {
        if cf_name.trim().is_empty() {
            return Err(revision_guarded_mutation_failed(
                CF_NAME,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "cross-CF batch has an empty column-family name: batch_index={batch_index}"
                ),
            ));
        }
        for (row_index, (key, _value)) in rows.iter().enumerate() {
            if key.is_empty() {
                return Err(revision_guarded_mutation_failed(
                    CF_NAME,
                    STORAGE_REVISION_GUARD_INVALID,
                    format!(
                        "cross-CF mutation key must be non-empty: batch_index={batch_index} row_index={row_index} cf={cf_name}"
                    ),
                ));
            }
            let count = mutation_counts
                .entry((cf_name.as_str(), key.as_slice()))
                .or_default();
            *count += 1;
            if *count != 1 {
                return Err(revision_guarded_mutation_failed(
                    CF_NAME,
                    STORAGE_REVISION_GUARD_INVALID,
                    format!(
                        "cross-CF mutation identities must be unique: cf={cf_name} key_len={} count={count}",
                        key.len()
                    ),
                ));
            }
        }
    }

    let mut unique_guards = BTreeSet::<(&str, &[u8])>::new();
    for (guard_index, guard) in guards.iter().enumerate() {
        if guard.cf_name.trim().is_empty() || guard.key.is_empty() {
            return Err(revision_guarded_mutation_failed(
                CF_NAME,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "cross-CF guard identity must be non-empty: guard_index={guard_index} cf_empty={} key_len={}",
                    guard.cf_name.trim().is_empty(),
                    guard.key.len()
                ),
            ));
        }
        if !unique_guards.insert((guard.cf_name.as_str(), guard.key.as_slice())) {
            return Err(revision_guarded_mutation_failed(
                CF_NAME,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "cross-CF guard identities must be unique: guard_index={guard_index} cf={} key_len={}",
                    guard.cf_name,
                    guard.key.len()
                ),
            ));
        }
        let matching_mutations = mutation_counts
            .get(&(guard.cf_name.as_str(), guard.key.as_slice()))
            .copied()
            .unwrap_or_default();
        if matching_mutations > 1 {
            return Err(revision_guarded_mutation_failed(
                CF_NAME,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "cross-CF guarded write permits at most one mutation for every guard: guard_index={guard_index} cf={} key_len={} matching_mutations={matching_mutations}",
                    guard.cf_name,
                    guard.key.len()
                ),
            ));
        }
    }
    Ok(())
}

fn commit_calyx_cross_cf_rows_if_revisions(
    vault: &SynapseCalyxVault,
    logical_guards: &[CfRevisionGuard],
    physical_guards: &[SynapseCalyxRevisionGuard],
    rows: Vec<SynapseCalyxCfWrite>,
) -> StorageResult<RevisionGuardedMutationOutcome> {
    const CF_NAME: &str = "<multi-cf>";
    let plan = plan_calyx_write_batches(CF_NAME, rows)?;
    let mut chunks = plan.chunks.into_iter();
    let chunk = chunks.next().ok_or_else(|| {
        revision_guarded_mutation_failed(
            CF_NAME,
            STORAGE_REVISION_GUARD_INVALID,
            "cross-CF revision-guarded Calyx mutation must contain at least one row".to_owned(),
        )
    })?;
    if chunks.next().is_some() {
        return Err(revision_guarded_mutation_failed(
            CF_NAME,
            STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE,
            format!(
                "cross-CF revision-guarded Calyx batch exceeds one atomic WAL record: total_rows={} total_payload_bytes={} max_payload_bytes={}; split would violate journal/projection atomicity",
                plan.total_rows, plan.total_payload_bytes, CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES
            ),
        ));
    }
    let outcome = vault
        .write_cf_batch_if_revisions(physical_guards.to_vec(), chunk.rows)
        .map_err(|source| {
            calyx_conditional_write_failed(
                CF_NAME,
                &format!(
                    "write cross-CF revision-guarded Calyx batch guards={} rows={} estimated_payload_bytes={} max_payload_bytes={}",
                    physical_guards.len(),
                    plan.total_rows,
                    plan.total_payload_bytes,
                    CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES
                ),
                &source,
            )
        })?;
    // The guarded Aster call is itself the durable WAL/MVCC boundary. A
    // second fallible flush here would turn a known applied commit into an
    // ambiguous ordinary error and make an append-only retry unsafe.
    validate_cross_cf_revision_guarded_outcome(logical_guards, physical_guards, &outcome)?;
    if let Some(conflict) = &outcome.conflict {
        let logical = &logical_guards[conflict.guard_index];
        tracing::warn!(
            code = "STORAGE_CALYX_REVISION_CONFLICT",
            cf = %logical.cf_name,
            committed_seq = outcome.committed_seq,
            guard_index = conflict.guard_index,
            guard_key_len = logical.key.len(),
            expected_revision_present = conflict.expected_revision_sha256.is_some(),
            actual_revision_present = conflict.actual_revision_sha256.is_some(),
            guard_count = logical_guards.len(),
            "cross-CF revision-guarded Calyx batch was not applied because a guarded value changed"
        );
    }
    Ok(RevisionGuardedMutationOutcome {
        applied: outcome.applied,
        committed_seq: outcome.committed_seq,
        actual_revisions_sha256: outcome.actual_revisions_sha256,
        committed_revisions_sha256: outcome.committed_revisions_sha256,
        conflict: outcome.conflict.map(|conflict| RevisionGuardConflict {
            guard_index: conflict.guard_index,
            key: logical_guards[conflict.guard_index].key.clone(),
            expected_revision_sha256: conflict.expected_revision_sha256,
            actual_revision_sha256: conflict.actual_revision_sha256,
        }),
    })
}

fn validate_cross_cf_revision_guarded_outcome(
    logical_guards: &[CfRevisionGuard],
    physical_guards: &[SynapseCalyxRevisionGuard],
    outcome: &SynapseCalyxMultiConditionalWriteOutcome,
) -> StorageResult<()> {
    const CF_NAME: &str = "<multi-cf>";
    let guard_count = logical_guards.len();
    let shape_valid = physical_guards.len() == guard_count
        && outcome.actual_revisions_sha256.len() == guard_count
        && if outcome.applied {
            outcome.conflict.is_none() && outcome.committed_revisions_sha256.len() == guard_count
        } else {
            outcome.committed_revisions_sha256.is_empty() && outcome.conflict.is_some()
        };
    if !shape_valid {
        return Err(revision_guarded_mutation_failed(
            CF_NAME,
            STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
            format!(
                "cross-CF guarded outcome shape violated contract: applied={} logical_guards={guard_count} physical_guards={} actual_revisions={} committed_revisions={} conflict_present={}",
                outcome.applied,
                physical_guards.len(),
                outcome.actual_revisions_sha256.len(),
                outcome.committed_revisions_sha256.len(),
                outcome.conflict.is_some()
            ),
        ));
    }
    let Some(conflict) = &outcome.conflict else {
        return Ok(());
    };
    let Some(logical_guard) = logical_guards.get(conflict.guard_index) else {
        return Err(revision_guarded_mutation_failed(
            CF_NAME,
            STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
            format!(
                "cross-CF guarded conflict index is out of bounds: guard_index={} guard_count={guard_count}",
                conflict.guard_index
            ),
        ));
    };
    let physical_guard = &physical_guards[conflict.guard_index];
    let actual_revision = outcome.actual_revisions_sha256[conflict.guard_index];
    if conflict.cf != physical_guard.cf
        || conflict.key != physical_guard.key
        || conflict.expected_revision_sha256 != logical_guard.expected_revision_sha256
        || conflict.actual_revision_sha256 != actual_revision
    {
        return Err(revision_guarded_mutation_failed(
            CF_NAME,
            STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
            format!(
                "cross-CF guarded conflict identity violated contract: guard_index={} cf_matches={} key_matches={} expected_matches={} actual_matches={}",
                conflict.guard_index,
                conflict.cf == physical_guard.cf,
                conflict.key == physical_guard.key,
                conflict.expected_revision_sha256 == logical_guard.expected_revision_sha256,
                conflict.actual_revision_sha256 == actual_revision
            ),
        ));
    }
    Ok(())
}

fn validate_revision_guarded_mutation(
    cf_name: &str,
    guards: &[RevisionGuard],
    deletes: &[Vec<u8>],
    puts: &[RawRow],
) -> StorageResult<()> {
    if guards.is_empty() {
        return Err(revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARD_INVALID,
            "revision-guarded mutation requires at least one logical guard".to_owned(),
        ));
    }

    let mut unique_guards = BTreeSet::new();
    for (guard_index, guard) in guards.iter().enumerate() {
        if guard.key.is_empty() {
            return Err(revision_guarded_mutation_failed(
                cf_name,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "revision-guarded mutation guard key must be non-empty: guard_index={guard_index}"
                ),
            ));
        }
        if !unique_guards.insert(guard.key.as_slice()) {
            return Err(revision_guarded_mutation_failed(
                cf_name,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "revision-guarded mutation guard keys must be unique: guard_index={guard_index} guard_key_len={}",
                    guard.key.len()
                ),
            ));
        }
    }

    let mut mutation_counts = BTreeMap::<&[u8], usize>::new();
    for key in deletes
        .iter()
        .map(Vec::as_slice)
        .chain(puts.iter().map(|(key, _)| key.as_slice()))
    {
        let count = mutation_counts.entry(key).or_default();
        *count += 1;
        if *count != 1 {
            return Err(revision_guarded_mutation_failed(
                cf_name,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "revision-guarded mutation keys must be unique across deletes and puts: mutation_key_len={}",
                    key.len()
                ),
            ));
        }
    }

    for (guard_index, guard) in guards.iter().enumerate() {
        let matching_mutations = mutation_counts
            .get(guard.key.as_slice())
            .copied()
            .unwrap_or_default();
        if matching_mutations > 1 {
            return Err(revision_guarded_mutation_failed(
                cf_name,
                STORAGE_REVISION_GUARD_INVALID,
                format!(
                    "revision-guarded mutation permits at most one delete or put for every guard: guard_index={guard_index} guard_key_len={} matching_mutations={matching_mutations} deletes={} puts={}",
                    guard.key.len(),
                    deletes.len(),
                    puts.len()
                ),
            ));
        }
    }
    Ok(())
}

fn commit_calyx_rows_if_revisions(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    logical_guards: &[RevisionGuard],
    physical_guards: &[SynapseCalyxRevisionGuard],
    rows: Vec<SynapseCalyxCfWrite>,
) -> StorageResult<RevisionGuardedMutationOutcome> {
    let plan = plan_calyx_write_batches(cf_name, rows)?;
    let mut chunks = plan.chunks.into_iter();
    let chunk = chunks.next().ok_or_else(|| {
        revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARD_INVALID,
            "revision-guarded Calyx mutation must contain at least one guarded row".to_owned(),
        )
    })?;
    if chunks.next().is_some() {
        return Err(revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE,
            format!(
                "revision-guarded Calyx batch exceeds one atomic WAL record: total_rows={} total_payload_bytes={} max_payload_bytes={}; split would violate compare-and-swap atomicity",
                plan.total_rows, plan.total_payload_bytes, CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES
            ),
        ));
    }
    let outcome = vault
        .write_cf_batch_if_revisions(physical_guards.to_vec(), chunk.rows)
        .map_err(|source| {
            calyx_conditional_write_failed(
                cf_name,
                &format!(
                    "write multi-key revision-guarded Calyx CF batch guards={} rows={} estimated_payload_bytes={} max_payload_bytes={}",
                    physical_guards.len(),
                    plan.total_rows,
                    plan.total_payload_bytes,
                    CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES
                ),
                &source,
            )
        })?;
    // `write_cf_batch_if_revisions` returns the durable commit outcome. Do
    // not overwrite that known outcome with a later maintenance-flush error.
    validate_revision_guarded_outcome(cf_name, logical_guards, physical_guards, &outcome)?;

    if let Some(conflict) = &outcome.conflict {
        tracing::warn!(
            code = "STORAGE_CALYX_REVISION_CONFLICT",
            cf = cf_name,
            committed_seq = outcome.committed_seq,
            guard_index = conflict.guard_index,
            guard_key_len = logical_guards[conflict.guard_index].key.len(),
            expected_revision_present = conflict.expected_revision_sha256.is_some(),
            actual_revision_present = conflict.actual_revision_sha256.is_some(),
            guard_count = logical_guards.len(),
            "multi-key revision-guarded Calyx CF batch was not applied because a guarded value changed"
        );
    }
    Ok(RevisionGuardedMutationOutcome {
        applied: outcome.applied,
        committed_seq: outcome.committed_seq,
        actual_revisions_sha256: outcome.actual_revisions_sha256,
        committed_revisions_sha256: outcome.committed_revisions_sha256,
        conflict: outcome.conflict.map(|conflict| RevisionGuardConflict {
            guard_index: conflict.guard_index,
            key: logical_guards[conflict.guard_index].key.clone(),
            expected_revision_sha256: conflict.expected_revision_sha256,
            actual_revision_sha256: conflict.actual_revision_sha256,
        }),
    })
}

fn validate_revision_guarded_outcome(
    cf_name: &str,
    logical_guards: &[RevisionGuard],
    physical_guards: &[SynapseCalyxRevisionGuard],
    outcome: &SynapseCalyxMultiConditionalWriteOutcome,
) -> StorageResult<()> {
    let guard_count = logical_guards.len();
    let shape_valid = physical_guards.len() == guard_count
        && outcome.actual_revisions_sha256.len() == guard_count
        && if outcome.applied {
            outcome.conflict.is_none() && outcome.committed_revisions_sha256.len() == guard_count
        } else {
            outcome.committed_revisions_sha256.is_empty() && outcome.conflict.is_some()
        };
    if !shape_valid {
        return Err(revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
            format!(
                "Calyx guarded-mutation outcome shape violated contract: applied={} logical_guards={guard_count} physical_guards={} actual_revisions={} committed_revisions={} conflict_present={}",
                outcome.applied,
                physical_guards.len(),
                outcome.actual_revisions_sha256.len(),
                outcome.committed_revisions_sha256.len(),
                outcome.conflict.is_some()
            ),
        ));
    }

    let Some(conflict) = &outcome.conflict else {
        return Ok(());
    };
    let Some(logical_guard) = logical_guards.get(conflict.guard_index) else {
        return Err(revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
            format!(
                "Calyx guarded-mutation conflict index is out of bounds: guard_index={} guard_count={guard_count}",
                conflict.guard_index
            ),
        ));
    };
    let physical_guard = &physical_guards[conflict.guard_index];
    let actual_revision = outcome.actual_revisions_sha256[conflict.guard_index];
    if conflict.cf != physical_guard.cf
        || conflict.key != physical_guard.key
        || conflict.expected_revision_sha256 != logical_guard.expected_revision_sha256
        || conflict.actual_revision_sha256 != actual_revision
    {
        return Err(revision_guarded_mutation_failed(
            cf_name,
            STORAGE_REVISION_GUARDED_OUTCOME_INVALID,
            format!(
                "Calyx guarded-mutation conflict identity violated contract: guard_index={} cf_matches={} key_matches={} expected_matches={} actual_matches={}",
                conflict.guard_index,
                conflict.cf == physical_guard.cf,
                conflict.key == physical_guard.key,
                conflict.expected_revision_sha256 == logical_guard.expected_revision_sha256,
                conflict.actual_revision_sha256 == actual_revision
            ),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct CalyxWriteBatchChunk {
    rows: Vec<SynapseCalyxCfWrite>,
    payload_bytes: usize,
}

#[derive(Debug)]
struct CalyxWriteBatchPlan {
    chunks: Vec<CalyxWriteBatchChunk>,
    total_rows: usize,
    total_payload_bytes: usize,
    largest_row_payload_bytes: usize,
}

fn plan_calyx_write_batches(
    cf_name: &str,
    rows: Vec<SynapseCalyxCfWrite>,
) -> StorageResult<CalyxWriteBatchPlan> {
    let total_rows = rows.len();
    if total_rows > u32::MAX as usize {
        return Err(calyx_write_failed_detail(
            cf_name,
            format!(
                "Calyx write batch row count exceeds u32 payload header: rows={total_rows} max_rows={}",
                u32::MAX
            ),
        ));
    }

    let mut chunks = Vec::new();
    let mut current_rows = Vec::new();
    let mut current_payload_bytes = CALYX_WRITE_BATCH_ROW_COUNT_BYTES;
    let mut total_payload_bytes = CALYX_WRITE_BATCH_ROW_COUNT_BYTES;
    let mut largest_row_payload_bytes = 0_usize;

    for row in rows {
        let row_payload_bytes = calyx_write_row_payload_bytes(cf_name, &row)?;
        largest_row_payload_bytes = largest_row_payload_bytes.max(row_payload_bytes);
        total_payload_bytes =
            checked_calyx_payload_add(cf_name, total_payload_bytes, row_payload_bytes)?;

        if CALYX_WRITE_BATCH_ROW_COUNT_BYTES
            .checked_add(row_payload_bytes)
            .is_none_or(|payload| payload > CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES)
        {
            tracing::error!(
                code = "STORAGE_CALYX_WRITE_BATCH_ROW_TOO_LARGE",
                cf = cf_name,
                row_payload_bytes,
                max_payload_bytes = CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES,
                wal_max_record_bytes = wal::MAX_RECORD_BYTES,
                headroom_bytes = CALYX_WRITE_BATCH_WAL_HEADROOM_BYTES,
                key_len_bytes = row.key.len(),
                value_len_bytes = row.value.len(),
                "Calyx CF write row cannot fit in one WAL-safe batch"
            );
            return Err(calyx_write_failed_detail(
                cf_name,
                format!(
                    "Calyx CF write row cannot fit in one WAL-safe batch: row_payload_bytes={row_payload_bytes} max_payload_bytes={} wal_max_record_bytes={} headroom_bytes={} key_len_bytes={} value_len_bytes={}",
                    CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES,
                    wal::MAX_RECORD_BYTES,
                    CALYX_WRITE_BATCH_WAL_HEADROOM_BYTES,
                    row.key.len(),
                    row.value.len()
                ),
            ));
        }

        if !current_rows.is_empty()
            && current_payload_bytes
                .checked_add(row_payload_bytes)
                .is_none_or(|payload| payload > CALYX_WRITE_BATCH_MAX_PAYLOAD_BYTES)
        {
            chunks.push(CalyxWriteBatchChunk {
                rows: std::mem::take(&mut current_rows),
                payload_bytes: current_payload_bytes,
            });
            current_payload_bytes = CALYX_WRITE_BATCH_ROW_COUNT_BYTES;
        }

        current_payload_bytes =
            checked_calyx_payload_add(cf_name, current_payload_bytes, row_payload_bytes)?;
        current_rows.push(row);
    }

    if !current_rows.is_empty() {
        chunks.push(CalyxWriteBatchChunk {
            rows: current_rows,
            payload_bytes: current_payload_bytes,
        });
    }

    Ok(CalyxWriteBatchPlan {
        chunks,
        total_rows,
        total_payload_bytes,
        largest_row_payload_bytes,
    })
}

fn calyx_write_row_payload_bytes(cf_name: &str, row: &SynapseCalyxCfWrite) -> StorageResult<usize> {
    u32::try_from(row.key.len()).map_err(|_error| {
        calyx_write_failed_detail(
            cf_name,
            format!(
                "Calyx CF write key exceeds u32 length prefix: key_len_bytes={}",
                row.key.len()
            ),
        )
    })?;
    u32::try_from(row.value.len()).map_err(|_error| {
        calyx_write_failed_detail(
            cf_name,
            format!(
                "Calyx CF write value exceeds u32 length prefix: value_len_bytes={}",
                row.value.len()
            ),
        )
    })?;
    CALYX_WRITE_BATCH_ROW_OVERHEAD_BYTES
        .checked_add(row.key.len())
        .and_then(|payload| payload.checked_add(row.value.len()))
        .ok_or_else(|| {
            calyx_write_failed_detail(
                cf_name,
                format!(
                    "Calyx CF write row payload size overflow: key_len_bytes={} value_len_bytes={}",
                    row.key.len(),
                    row.value.len()
                ),
            )
        })
}

fn checked_calyx_payload_add(cf_name: &str, left: usize, right: usize) -> StorageResult<usize> {
    left.checked_add(right).ok_or_else(|| {
        calyx_write_failed_detail(
            cf_name,
            format!("Calyx CF write batch payload size overflow: left={left} right={right}"),
        )
    })
}

fn calyx_clock_now_for_read(vault: &SynapseCalyxVault, cf_name: &str) -> StorageResult<u64> {
    vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))
}

fn calyx_clock_now_for_write(vault: &SynapseCalyxVault, cf_name: &str) -> StorageResult<u64> {
    vault
        .clock_now_ms()
        .map_err(|source| calyx_write_failed(cf_name, "read Calyx vault clock", &source))
}

fn calyx_collection_id_for_cf(cf_name: &str) -> Option<u64> {
    for (offset, known_cf) in (1_u64..).zip(cf::ALL_COLUMN_FAMILIES) {
        if cf_name == known_cf {
            return Some(CALYX_COLLECTION_ID_BASE | offset);
        }
    }
    None
}

enum CalyxStorageSnapshotTarget {
    SynapseKv { physical_key: Vec<u8> },
    Native { cf: ColumnFamily },
}

fn calyx_storage_snapshot_target(
    cf_name: &str,
    key: &[u8],
) -> StorageResult<CalyxStorageSnapshotTarget> {
    if let Some(collection_id) = calyx_collection_id_for_cf(cf_name) {
        return Ok(CalyxStorageSnapshotTarget::SynapseKv {
            physical_key: encode_calyx_key_for_read(cf_name, collection_id, key)?,
        });
    }

    let normalized = cf_name.to_ascii_lowercase();
    let native_cf = ColumnFamily::from_name(&normalized).filter(|cf| cf.name() == normalized);
    native_cf.map_or_else(
        || {
            tracing::error!(
                code = "SYNAPSE_CALYX_SNAPSHOT_CF_UNKNOWN",
                requested_cf = cf_name,
                "snapshot point-read rejected a name outside both closed CF catalogs"
            );
            Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "SYNAPSE_CALYX_SNAPSHOT_CF_UNKNOWN: cf_name={cf_name:?} is neither a logical Synapse collection nor a native Calyx column family; remediation=pass an exact logical CF name or a native Calyx family name such as Graph"
                ),
            })
        },
        |cf| Ok(CalyxStorageSnapshotTarget::Native { cf }),
    )
}

fn read_calyx_storage_snapshot_target(
    vault: &SynapseCalyxVault,
    pinned: PinnedStorageSnapshot,
    cf_name: &str,
    key: &[u8],
) -> StorageResult<(Option<Vec<u8>>, bool)> {
    match calyx_storage_snapshot_target(cf_name, key)? {
        CalyxStorageSnapshotTarget::SynapseKv { physical_key } => vault
            .read_cf_snapshot(pinned.snapshot, ColumnFamily::Kv, &physical_key)
            .map(|physical| (physical, false))
            .map_err(|source| {
                calyx_read_failed(cf_name, "read Calyx KV row at pinned snapshot", &source)
            }),
        CalyxStorageSnapshotTarget::Native { cf } => vault
            .read_cf_snapshot(pinned.snapshot, cf, key)
            .map(|physical| (physical, true))
            .map_err(|source| {
                calyx_read_failed(
                    cf_name,
                    "read native Calyx CF row at pinned snapshot",
                    &source,
                )
            }),
    }
}

fn native_snapshot_readback(
    lease_id: u64,
    pinned: PinnedStorageSnapshot,
    current_seq: u64,
    cf_name: &str,
    physical: &[u8],
) -> CalyxStorageSnapshotReadback {
    CalyxStorageSnapshotReadback {
        lease_id,
        snapshot_seq: pinned.snapshot.seq(),
        current_seq,
        opened_at_unix_ms: pinned.opened_at_unix_ms,
        expires_at_unix_ms: pinned.snapshot.lease().expires_at(),
        cf_name: cf_name.to_owned(),
        physical_present: true,
        logical_present: true,
        expired_at_snapshot: false,
        written_at_unix_ms: None,
        retention_expires_at_unix_ms: None,
        payload_len_bytes: Some(u64::try_from(physical.len()).unwrap_or(u64::MAX)),
        payload_sha256: Some(sha256_hex(physical)),
    }
}

fn calyx_collection_id_for_cf_read(cf_name: &str) -> StorageResult<u64> {
    calyx_collection_id_for_cf(cf_name).ok_or_else(|| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail: "column family name is not part of the Synapse storage schema".to_owned(),
    })
}

fn calyx_collection_id_for_cf_write(cf_name: &str) -> StorageResult<u64> {
    calyx_collection_id_for_cf(cf_name).ok_or_else(|| StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
        detail: "column family name is not part of the Synapse storage schema".to_owned(),
    })
}

#[derive(Debug)]
struct CalyxRetentionLiveEntry {
    full_key: Vec<u8>,
    user_key: Vec<u8>,
    live_bytes: u64,
    written_at_ms: u64,
    /// Digest of the exact stored bytes this entry's eviction decision was
    /// taken from (#2060). See [`calyx_gc_row_digest`].
    value_digest: u64,
}

/// One row the retention sweep proposes to delete, carrying the evidence its
/// decision was taken from (#2060).
///
/// The sweep no longer reads the whole namespace under one row-guard hold, so
/// the rows it decided from were observed at a succession of committed
/// sequences rather than at one instant. A proposal is therefore not authority
/// to delete on its own: it is re-checked against the row's *current* bytes
/// under a short point-read guard before the tombstone is written, and a row
/// whose bytes changed since the page that proposed it is retained.
#[derive(Debug)]
struct CalyxGcEvictionCandidate {
    full_key: Vec<u8>,
    value_digest: u64,
}

/// What re-checking eviction proposals against current bytes decided (#2060).
///
/// Reported rather than swallowed: a non-zero `superseded_rows` is the exact
/// evidence that paging the sweep widened the observe-to-delete window enough
/// to matter, and a non-zero `vanished_rows` means another writer deleted the
/// row first.
#[derive(Clone, Copy, Debug, Default)]
struct CalyxGcEvictionRevalidation {
    checked: u64,
    superseded: u64,
    vanished: u64,
}

#[derive(Debug)]
struct CalyxRetentionState {
    /// Exact non-expired, non-derived-referenced row count observed by the
    /// pinned sweep. This remains scalar for policy-protected families because
    /// those families can never consume per-row eviction candidates.
    before_live_rows: u64,
    live_entries: Vec<CalyxRetentionLiveEntry>,
    /// Expired rows the sweep proposed, before the pre-delete re-check.
    expired_candidates: Vec<CalyxGcEvictionCandidate>,
    tombstones: Vec<SynapseCalyxCfWrite>,
    before_live_bytes: u64,
    expired_rows: u64,
    /// Rows GC declined to consider because a live derived constellation still
    /// points at them (#1882). Neither evicted nor counted toward the caps.
    retained_referenced_rows: u64,
    /// Provenance of the paged sweep the state was folded from (#2060).
    sweep: CalyxKvSweep,
    /// Outcome of re-checking every eviction proposal (#2060).
    revalidation: CalyxGcEvictionRevalidation,
}

#[derive(Clone, Copy, Debug)]
struct CalyxGcBudget {
    cf_name: &'static str,
    soft_cap: u64,
    hard_cap: u64,
    unit: CalyxGcUnit,
    protected: bool,
}

#[derive(Clone, Copy, Debug)]
enum CalyxGcUnit {
    Bytes,
    Rows,
}

impl CalyxGcUnit {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Rows => "rows",
        }
    }
}

#[derive(Debug)]
struct CalyxGcCapOutcome {
    after_value: u64,
    cap_evicted_rows: u64,
    eviction_skipped_reason: Option<&'static str>,
}

/// This process's committed private memory, or a structured failure.
///
/// Fails closed rather than substituting working set. Working set is the metric
/// #2122 showed to be wrong by 2-5x in the direction that suppresses the alarm
/// — it oscillated 4,731-12,158 MB while private commit rose monotonically
/// 25,599 -> 28,206 MB — so falling back to it would let a pressure decision be
/// made from a number known to be misleading, while looking like it succeeded.
fn calyx_process_private_bytes() -> StorageResult<u64> {
    synapse_calyx::process_private_bytes().map_err(|source| StorageError::WriteFailed {
        cf_name: CALYX_GC_CF.to_owned(),
        detail: format!(
            "read process committed private memory for snapshot-version GC pressure: {source}"
        ),
    })
}

fn calyx_gc_default_budgets() -> StorageResult<Vec<CalyxGcBudget>> {
    validate_calyx_retention_cap_coupling()?;
    DEFAULTS
        .iter()
        .copied()
        .map(|retention| {
            let (soft_cap, hard_cap) = calyx_retention_cap_bytes_for_write(retention)?;
            calyx_gc_budget(retention.cf, soft_cap, hard_cap, CalyxGcUnit::Bytes)
        })
        .collect()
}

fn calyx_gc_row_budget(
    cf_name: &'static str,
    soft_cap_rows: u64,
    hard_cap_rows: u64,
) -> StorageResult<CalyxGcBudget> {
    calyx_gc_budget(cf_name, soft_cap_rows, hard_cap_rows, CalyxGcUnit::Rows)
}

fn calyx_gc_budget(
    cf_name: &'static str,
    soft_cap: u64,
    hard_cap: u64,
    unit: CalyxGcUnit,
) -> StorageResult<CalyxGcBudget> {
    calyx_collection_id_for_cf_write(cf_name)?;
    if soft_cap == 0 || hard_cap == 0 {
        return Err(calyx_write_failed_detail(
            cf_name,
            format!(
                "invalid Calyx GC {} cap: soft_cap={soft_cap} hard_cap={hard_cap}; both caps must be non-zero",
                unit.as_str()
            ),
        ));
    }
    if hard_cap < soft_cap {
        return Err(calyx_write_failed_detail(
            cf_name,
            format!(
                "invalid Calyx GC {} cap: hard_cap={hard_cap} is below soft_cap={soft_cap}",
                unit.as_str()
            ),
        ));
    }
    Ok(CalyxGcBudget {
        cf_name,
        soft_cap,
        hard_cap,
        unit,
        protected: calyx_cf_protected_from_auto_delete(cf_name),
    })
}

/// Builds the set of source rows that a live derived constellation still points
/// at, keyed by the Synapse column family it names.
///
/// A constellation's `CxId` is content-addressed over the exact input bytes it
/// was measured from, so once the source row is gone those bytes exist nowhere
/// and the row can never be re-derived, re-encoded to a newer lens generation,
/// or audited against its own derivation. Before #1882, eviction order decided
/// this by accident: on the production vault 227 derived rows had already
/// outlived their sources, including every `syn-process-v1` and
/// `syn-observation-v1` row.
///
/// Built once per GC tick and shared across every CF budget, so the Base scan
/// is paid once rather than per column family.
///
/// One source key's exact byte range in one
/// [`PackedSourceReferenceKeys::chunks`] allocation.
///
/// One fixed-width chunk index plus two offsets replace a `Vec` allocation and
/// a B-tree node per reference. The production corpus has more than 1.5
/// million references, so the ownership shape matters more than lookup's
/// constant factor.
#[derive(Clone, Copy, Debug)]
struct PackedSourceReferenceRange(u64);

const PACKED_SOURCE_REFERENCE_LENGTH_BITS: u32 = 16;
const PACKED_SOURCE_REFERENCE_START_BITS: u32 = 20;
const PACKED_SOURCE_REFERENCE_CHUNK_BITS: u32 =
    u64::BITS - PACKED_SOURCE_REFERENCE_LENGTH_BITS - PACKED_SOURCE_REFERENCE_START_BITS;
const PACKED_SOURCE_REFERENCE_LENGTH_MASK: u64 = (1_u64 << PACKED_SOURCE_REFERENCE_LENGTH_BITS) - 1;
const PACKED_SOURCE_REFERENCE_START_MASK: u64 = (1_u64 << PACKED_SOURCE_REFERENCE_START_BITS) - 1;
const PACKED_SOURCE_REFERENCE_CHUNK_MASK: u64 = (1_u64 << PACKED_SOURCE_REFERENCE_CHUNK_BITS) - 1;

impl PackedSourceReferenceRange {
    fn new(cf_name: &str, chunk: usize, start: usize, len: usize) -> Result<Self, String> {
        let chunk = u64::try_from(chunk)
            .map_err(|_| format!("source-reference chunk index for {cf_name} does not fit u64"))?;
        let start = u64::try_from(start)
            .map_err(|_| format!("source-reference chunk offset for {cf_name} does not fit u64"))?;
        let len = u64::try_from(len)
            .map_err(|_| format!("source-reference key length for {cf_name} does not fit u64"))?;
        if chunk > PACKED_SOURCE_REFERENCE_CHUNK_MASK {
            return Err(format!(
                "source-reference arena for {cf_name} requires chunk index {chunk}, exceeding the {PACKED_SOURCE_REFERENCE_CHUNK_BITS}-bit exact packed representation"
            ));
        }
        if start > PACKED_SOURCE_REFERENCE_START_MASK {
            return Err(format!(
                "source-reference chunk offset for {cf_name} is {start}, exceeding the {PACKED_SOURCE_REFERENCE_START_BITS}-bit exact packed representation"
            ));
        }
        if len == 0 || len > PACKED_SOURCE_REFERENCE_LENGTH_MASK {
            return Err(format!(
                "source-reference key length for {cf_name} is {len}, outside the exact 1..={PACKED_SOURCE_REFERENCE_LENGTH_MASK} byte storage envelope"
            ));
        }
        Ok(Self(
            (chunk << (PACKED_SOURCE_REFERENCE_START_BITS + PACKED_SOURCE_REFERENCE_LENGTH_BITS))
                | (start << PACKED_SOURCE_REFERENCE_LENGTH_BITS)
                | len,
        ))
    }

    const fn chunk(self) -> usize {
        ((self.0 >> (PACKED_SOURCE_REFERENCE_START_BITS + PACKED_SOURCE_REFERENCE_LENGTH_BITS))
            & PACKED_SOURCE_REFERENCE_CHUNK_MASK) as usize
    }

    const fn start(self) -> usize {
        ((self.0 >> PACKED_SOURCE_REFERENCE_LENGTH_BITS) & PACKED_SOURCE_REFERENCE_START_MASK)
            as usize
    }

    const fn end(self) -> usize {
        self.start() + (self.0 & PACKED_SOURCE_REFERENCE_LENGTH_MASK) as usize
    }
}

const PACKED_SOURCE_REFERENCE_CHUNK_BYTES: usize = 1024 * 1024;
const PACKED_SOURCE_REFERENCE_RANGE_RESERVE_ROWS: usize = 65_536;

/// Exact source-key membership backed by bounded byte chunks plus compact
/// ranges.
///
/// Base is ordered by constellation identity rather than source key, so keys
/// are appended during the pinned stream and only the fixed-width ranges are
/// sorted/deduplicated afterward. A single growing `Vec<u8>` is deliberately
/// not used here: when it doubles near 64-128 MiB, the allocator must own the
/// old and new buffers simultaneously while copying, creating a short
/// corpus-proportional commit spike even though steady-state ownership is
/// compact. One-MiB chunks bound that overlap while retaining exact binary
/// search; an individual key larger than a chunk gets one exact-size chunk.
/// There is no probabilistic filter and no false-positive deletion risk.
#[derive(Default)]
struct PackedSourceReferenceKeys {
    chunks: Vec<Vec<u8>>,
    ranges: Vec<PackedSourceReferenceRange>,
}

impl PackedSourceReferenceKeys {
    fn push(&mut self, cf_name: &str, key: &[u8]) -> Result<(), String> {
        if self.ranges.len() == self.ranges.capacity() {
            self.ranges
                .try_reserve_exact(PACKED_SOURCE_REFERENCE_RANGE_RESERVE_ROWS)
                .map_err(|source| {
                    format!(
                        "reserve the next bounded block of {PACKED_SOURCE_REFERENCE_RANGE_RESERVE_ROWS} exact source-reference ranges for {cf_name} failed: {source}"
                    )
                })?;
        }

        let needs_chunk = self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.capacity().saturating_sub(chunk.len()) < key.len());
        if needs_chunk {
            let chunk_capacity = PACKED_SOURCE_REFERENCE_CHUNK_BYTES.max(key.len());
            let mut chunk = Vec::new();
            chunk.try_reserve_exact(chunk_capacity).map_err(|source| {
                format!(
                    "reserve a {chunk_capacity}-byte source-reference chunk for {cf_name} failed: {source}"
                )
            })?;
            self.chunks.try_reserve(1).map_err(|source| {
                format!("reserve one source-reference chunk slot for {cf_name} failed: {source}")
            })?;
            self.chunks.push(chunk);
        }

        let chunk_index = self.chunks.len() - 1;
        let chunk = self
            .chunks
            .last_mut()
            .ok_or_else(|| format!("source-reference chunk allocation vanished for {cf_name}"))?;
        let start = chunk.len();
        let range = PackedSourceReferenceRange::new(cf_name, chunk_index, start, key.len())?;
        chunk.extend_from_slice(key);
        self.ranges.push(range);
        Ok(())
    }

    fn truncate_chunks_to_reachable_ranges(
        &mut self,
        cf_name: &str,
        retained_chunks: usize,
    ) -> Result<(), String> {
        self.chunks.truncate(retained_chunks);
        let mut used_bytes_by_chunk = Vec::new();
        used_bytes_by_chunk
            .try_reserve_exact(retained_chunks)
            .map_err(|source| {
                format!(
                    "reserve {retained_chunks} source-reference chunk watermarks for {cf_name} failed: {source}"
                )
            })?;
        used_bytes_by_chunk.resize(retained_chunks, 0_usize);
        for range in &self.ranges {
            let used = used_bytes_by_chunk.get_mut(range.chunk()).ok_or_else(|| {
                format!(
                    "source-reference compacted range names absent chunk {} for {cf_name}",
                    range.chunk()
                )
            })?;
            *used = (*used).max(range.end());
        }
        for (chunk_index, (chunk, used)) in
            self.chunks.iter_mut().zip(used_bytes_by_chunk).enumerate()
        {
            if used == 0 || used > chunk.len() {
                return Err(format!(
                    "source-reference compacted chunk {chunk_index} for {cf_name} has invalid reachable bytes {used} with physical length {}",
                    chunk.len()
                ));
            }
            chunk.truncate(used);
        }
        Ok(())
    }

    fn validate_exact_packed_bytes(&self, cf_name: &str) -> Result<(), String> {
        let reachable_key_bytes = self.ranges.iter().try_fold(0_usize, |total, range| {
            total
                .checked_add(range.end().saturating_sub(range.start()))
                .ok_or_else(|| {
                    format!("source-reference reachable key bytes overflowed for {cf_name}")
                })
        })?;
        let packed_key_bytes = self.packed_bytes();
        if packed_key_bytes != reachable_key_bytes {
            return Err(format!(
                "source-reference compaction for {cf_name} retained {packed_key_bytes} physical bytes but ranges address {reachable_key_bytes} bytes"
            ));
        }
        Ok(())
    }

    fn sort_and_dedup(&mut self, cf_name: &str) -> Result<(), String> {
        let chunks = self.chunks.as_slice();
        self.ranges.sort_unstable_by(|left, right| {
            packed_source_reference_key(chunks, *left)
                .cmp(packed_source_reference_key(chunks, *right))
        });
        self.ranges.dedup_by(|left, right| {
            packed_source_reference_key(chunks, *left)
                == packed_source_reference_key(chunks, *right)
        });

        // Deduplicating only the range index is not enough: duplicate source
        // keys remain physically owned by the byte arena even after no range
        // can reach them. On the production vault that left hundreds of
        // thousands of dead key copies resident in the long-lived GC cache.
        // Compact the unique keys in original physical order, in place. The
        // destination never advances beyond the source, so no second
        // corpus-sized arena is allocated and `copy_within` handles overlap.
        self.ranges
            .sort_unstable_by_key(|range| (range.chunk(), range.start()));
        let mut destination_chunk = 0usize;
        let mut destination_start = 0usize;
        for index in 0..self.ranges.len() {
            let source = self.ranges[index];
            let source_len = source.end().saturating_sub(source.start());
            while self
                .chunks
                .get(destination_chunk)
                .is_some_and(|chunk| chunk.len().saturating_sub(destination_start) < source_len)
            {
                destination_chunk = destination_chunk.checked_add(1).ok_or_else(|| {
                    format!(
                        "source-reference compaction destination chunk overflowed for {cf_name}"
                    )
                })?;
                destination_start = 0;
            }
            let destination = self.chunks.get(destination_chunk).ok_or_else(|| {
                format!(
                    "source-reference compaction exhausted the existing arena for {cf_name}: destination_chunk={destination_chunk} source_chunk={} source_start={} source_len={source_len}",
                    source.chunk(),
                    source.start()
                )
            })?;
            if destination.len().saturating_sub(destination_start) < source_len {
                return Err(format!(
                    "source-reference compaction destination for {cf_name} has {} bytes but needs {source_len}",
                    destination.len().saturating_sub(destination_start)
                ));
            }
            if destination_chunk > source.chunk()
                || (destination_chunk == source.chunk() && destination_start > source.start())
            {
                return Err(format!(
                    "source-reference in-place compaction moved ahead of unread source bytes for {cf_name}: destination={destination_chunk}:{destination_start} source={}:{}",
                    source.chunk(),
                    source.start()
                ));
            }

            if destination_chunk == source.chunk() {
                self.chunks[destination_chunk]
                    .copy_within(source.start()..source.end(), destination_start);
            } else {
                let (destination_chunks, source_chunks) = self.chunks.split_at_mut(source.chunk());
                let destination = destination_chunks.get_mut(destination_chunk).ok_or_else(|| {
                    format!(
                        "source-reference destination chunk {destination_chunk} vanished while compacting {cf_name}"
                    )
                })?;
                let source_chunk = source_chunks.first().ok_or_else(|| {
                    format!(
                        "source-reference source chunk {} vanished while compacting {cf_name}",
                        source.chunk()
                    )
                })?;
                destination[destination_start..destination_start + source_len]
                    .copy_from_slice(&source_chunk[source.start()..source.end()]);
            }
            self.ranges[index] = PackedSourceReferenceRange::new(
                cf_name,
                destination_chunk,
                destination_start,
                source_len,
            )?;
            destination_start = destination_start.checked_add(source_len).ok_or_else(|| {
                format!("source-reference destination offset overflowed while compacting {cf_name}")
            })?;
        }

        if self.ranges.is_empty() {
            self.chunks.clear();
        } else {
            let retained_chunks = destination_chunk.checked_add(1).ok_or_else(|| {
                format!("source-reference retained chunk count overflowed for {cf_name}")
            })?;
            // Keys never straddle chunks. When the next key does not fit in
            // the current destination chunk, compaction advances to the next
            // chunk and leaves a short unused tail behind. Truncating only the
            // final chunk therefore made `packed_bytes()` count those tails
            // even though no range could address them. The durable artifact
            // correctly streams only addressable keys, so its planned length
            // diverged from the encoded length on real variable-sized keys.
            // Derive every retained chunk's exact reachable end from the
            // rewritten ranges, then remove all unreachable tails.
            self.truncate_chunks_to_reachable_ranges(cf_name, retained_chunks)?;
        }
        self.validate_exact_packed_bytes(cf_name)?;
        let chunks = self.chunks.as_slice();
        self.ranges.sort_unstable_by(|left, right| {
            packed_source_reference_key(chunks, *left)
                .cmp(packed_source_reference_key(chunks, *right))
        });
        self.ranges.shrink_to_fit();
        for chunk in &mut self.chunks {
            chunk.shrink_to_fit();
        }
        self.chunks.shrink_to_fit();
        Ok(())
    }

    const fn len(&self) -> usize {
        self.ranges.len()
    }

    fn packed_bytes(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    fn packed_capacity_bytes(&self) -> usize {
        self.chunks.iter().map(Vec::capacity).sum()
    }

    const fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    const fn range_bytes(&self) -> usize {
        self.ranges.len() * size_of::<PackedSourceReferenceRange>()
    }

    const fn range_capacity_bytes(&self) -> usize {
        self.ranges.capacity() * size_of::<PackedSourceReferenceRange>()
    }
}

fn packed_source_reference_key(chunks: &[Vec<u8>], range: PackedSourceReferenceRange) -> &[u8] {
    &chunks[range.chunk()][range.start()..range.end()]
}

/// Source column family -> exact source row keys a live derived constellation
/// still points at. The GC tick's allocation-compact protection set (#1882).
type DerivedSourceReferences = BTreeMap<String, PackedSourceReferenceKeys>;

const CALYX_GC_SOURCE_CENSUS_FULL_BASELINE: &str = "full_baseline";
const CALYX_GC_SOURCE_CENSUS_INCREMENTAL_DELTA: &str = "incremental_delta";
const CALYX_GC_SOURCE_CENSUS_UNCHANGED: &str = "unchanged";
const CALYX_GC_SOURCE_CENSUS_FULL_REBASE: &str = "full_rebase";
const CALYX_GC_SOURCE_CENSUS_REBASE_TOMBSTONE: &str = "base_tombstone";
const CALYX_GC_SOURCE_CENSUS_REBASE_DELTA_BOUND: &str = "delta_reference_bound";
const CALYX_GC_SOURCE_CENSUS_REBASE_HISTORY_GAP: &str = "changed_key_history_gap";
const CALYX_GC_SOURCE_CENSUS_REBASE_OUT_OF_BAND: &str = "base_out_of_band_change";
const CALYX_GC_SOURCE_CENSUS_MAPPED_REUSE: &str = "mapped_generation_reuse";
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIR: &str = "gc-source-census-v1";
const CALYX_GC_SOURCE_CENSUS_CURRENT_FILE: &str = "CURRENT";
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_PREFIX: &str = "source-census-";
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_SUFFIX: &str = ".idx";
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_LABEL: &str = "Synapse GC source-census artifact";
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAGIC: &[u8; 16] = b"SYN-GC-REF-IDX1\0";
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_VERSION: u32 = 1;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES: usize = 192;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES_U32: u32 = 192;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES: usize = 48;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES_U32: u32 = 48;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES: usize = 8;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES_U32: u32 = 8;
const CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAX_CFS: usize = 1_024;
static CALYX_GC_SOURCE_CENSUS_NEXT_ARTIFACT_ID: AtomicU64 = AtomicU64::new(0);

/// Maximum number of post-baseline exact references retained in individually
/// allocated ordered sets. Crossing the bound rebuilds the packed baseline at
/// one pinned sequence, so a long-lived process cannot accumulate an unbounded
/// remembered-set overhead.
const CALYX_GC_SOURCE_CENSUS_MAX_DELTA_REFERENCES: usize = 65_536;

struct MappedSourceReferenceSection {
    rows: usize,
    ranges_offset: usize,
    data_offset: usize,
}

struct MappedDerivedSourceReferences {
    artifact: MmapColumn,
    pinned_seq: u64,
    base_last_commit_seq: u64,
    base_out_of_band_epoch: u64,
    sections: BTreeMap<String, MappedSourceReferenceSection>,
    metrics: DerivedSourceReferenceMetrics,
}

impl MappedDerivedSourceReferences {
    fn contains(&self, cf_name: &str, key: &[u8]) -> bool {
        let Some(section) = self.sections.get(cf_name) else {
            return false;
        };
        let bytes = self.artifact.as_bytes();
        let mut low = 0_usize;
        let mut high = section.rows;
        while low < high {
            let middle = low + ((high - low) / 2);
            let range_offset =
                section.ranges_offset + (middle * CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES);
            let key_offset = match usize::try_from(read_artifact_u32(bytes, range_offset)) {
                Ok(offset) => offset,
                Err(source) => {
                    tracing::error!(
                        code = "STORAGE_CALYX_GC_SOURCE_CENSUS_MAPPED_OFFSET_INVALID",
                        path = %self.artifact.path().display(),
                        cf = cf_name,
                        row = middle,
                        error = %source,
                        "validated source-census mapping produced an unrepresentable offset; retaining the source row and refusing deletion authority"
                    );
                    return true;
                }
            };
            let key_len = usize::from(read_artifact_u16(bytes, range_offset + 4));
            let start = section.data_offset.saturating_add(key_offset);
            let end = start.saturating_add(key_len);
            let Some(candidate) = bytes.get(start..end) else {
                // The complete mapping is validated before this structure can
                // exist. A later impossible bounds failure must retain the
                // source row instead of authorizing deletion.
                tracing::error!(
                    code = "STORAGE_CALYX_GC_SOURCE_CENSUS_MAPPED_RANGE_INVALID",
                    path = %self.artifact.path().display(),
                    cf = cf_name,
                    row = middle,
                    key_offset,
                    key_len,
                    "validated source-census mapping produced an out-of-bounds range; retaining the source row and refusing deletion authority"
                );
                return true;
            };
            match candidate.cmp(key) {
                CmpOrdering::Less => low = middle + 1,
                CmpOrdering::Equal => return true,
                CmpOrdering::Greater => high = middle,
            }
        }
        false
    }
}

fn read_artifact_u16(bytes: &[u8], offset: usize) -> u16 {
    let mut raw = [0_u8; 2];
    if let Some(value) = bytes.get(offset..offset.saturating_add(raw.len())) {
        raw.copy_from_slice(value);
    }
    u16::from_le_bytes(raw)
}

fn read_artifact_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut raw = [0_u8; 4];
    if let Some(value) = bytes.get(offset..offset.saturating_add(raw.len())) {
        raw.copy_from_slice(value);
    }
    u32::from_le_bytes(raw)
}

fn read_artifact_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut raw = [0_u8; 8];
    if let Some(value) = bytes.get(offset..offset.saturating_add(raw.len())) {
        raw.copy_from_slice(value);
    }
    u64::from_le_bytes(raw)
}

/// Exact reachability at one committed sequence.
///
/// The corpus-sized baseline is an immutable, checksummed, read-only mapping;
/// the OS can evict its cold pages instead of charging ~100 MiB of private heap
/// to an idle daemon. Changes after that baseline are normally tiny and live in
/// ordered sets so each tick updates only changed references. Tombstones
/// invalidate the additive representation and force an exact rebase.
struct DerivedSourceReferenceIndex {
    baseline: MappedDerivedSourceReferences,
    delta: BTreeMap<String, BTreeSet<Vec<u8>>>,
}

impl DerivedSourceReferenceIndex {
    fn contains(&self, cf_name: &str, key: &[u8]) -> bool {
        self.baseline.contains(cf_name, key)
            || self
                .delta
                .get(cf_name)
                .is_some_and(|keys| keys.contains(key))
    }

    fn insert_delta(&mut self, cf_name: String, key: Vec<u8>) {
        if self.baseline.contains(&cf_name, &key) {
            return;
        }
        self.delta.entry(cf_name).or_default().insert(key);
    }

    fn delta_reference_count(&self) -> usize {
        self.delta.values().map(BTreeSet::len).sum()
    }

    fn referenced_column_families(&self) -> usize {
        self.baseline
            .sections
            .keys()
            .chain(self.delta.keys())
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn referenced_rows(&self) -> StorageResult<u64> {
        let baseline = self.baseline.metrics.rows;
        let delta = calyx_len_to_u64(
            CALYX_GC_CF,
            "Calyx GC delta protected source rows",
            self.delta_reference_count(),
        )?;
        baseline.checked_add(delta).ok_or_else(|| {
            calyx_write_failed_detail(
                CALYX_GC_CF,
                "Calyx GC protected source row count overflowed u64",
            )
        })
    }
}

struct CalyxGcSourceCensusCache {
    pinned_seq: u64,
    referenced: DerivedSourceReferenceIndex,
}

#[derive(Clone, Copy)]
struct DerivedSourceCensusProvenance {
    mode: &'static str,
    pinned_seq: u64,
    previous_pinned_seq: Option<u64>,
    pages: u64,
    base_rows_visited: u64,
    changed_base_keys: u64,
    rebase_reason: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Default)]
struct DerivedSourceReferenceMetrics {
    rows: u64,
    key_bytes: u64,
    key_capacity_bytes: u64,
    chunks: u64,
    range_bytes: u64,
    range_capacity_bytes: u64,
}

fn add_derived_source_reference_metric(
    total: &mut u64,
    label: &'static str,
    value: usize,
) -> StorageResult<()> {
    let value = calyx_len_to_u64(CALYX_GC_CF, label, value)?;
    *total = total
        .checked_add(value)
        .ok_or_else(|| calyx_write_failed_detail(CALYX_GC_CF, format!("{label} overflowed u64")))?;
    Ok(())
}

fn derived_source_reference_metrics(
    referenced: &DerivedSourceReferences,
) -> StorageResult<DerivedSourceReferenceMetrics> {
    let mut metrics = DerivedSourceReferenceMetrics::default();
    for keys in referenced.values() {
        add_derived_source_reference_metric(
            &mut metrics.rows,
            "Calyx GC protected source rows",
            keys.len(),
        )?;
        add_derived_source_reference_metric(
            &mut metrics.key_bytes,
            "Calyx GC packed source-reference key bytes",
            keys.packed_bytes(),
        )?;
        add_derived_source_reference_metric(
            &mut metrics.key_capacity_bytes,
            "Calyx GC packed source-reference key capacity bytes",
            keys.packed_capacity_bytes(),
        )?;
        add_derived_source_reference_metric(
            &mut metrics.chunks,
            "Calyx GC packed source-reference chunks",
            keys.chunk_count(),
        )?;
        add_derived_source_reference_metric(
            &mut metrics.range_bytes,
            "Calyx GC packed source-reference range bytes",
            keys.range_bytes(),
        )?;
        add_derived_source_reference_metric(
            &mut metrics.range_capacity_bytes,
            "Calyx GC packed source-reference range capacity bytes",
            keys.range_capacity_bytes(),
        )?;
    }
    Ok(metrics)
}

struct SourceCensusArtifactSectionLayout {
    cf_name: String,
    name_offset: usize,
    rows: usize,
    ranges_offset: usize,
    data_offset: usize,
    data_len: usize,
}

struct SourceCensusArtifactLayout {
    sections: Vec<SourceCensusArtifactSectionLayout>,
    rows: u64,
    file_len: usize,
}

fn source_census_artifact_dir(vault: &SynapseCalyxVault) -> PathBuf {
    vault
        .vault_dir()
        .join("derived")
        .join(CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIR)
}

fn source_census_current_path(vault: &SynapseCalyxVault) -> PathBuf {
    source_census_artifact_dir(vault).join(CALYX_GC_SOURCE_CENSUS_CURRENT_FILE)
}

fn source_census_vault_id_sha256(vault: &SynapseCalyxVault) -> [u8; 32] {
    Sha256::digest(vault.vault_id().as_bytes()).into()
}

fn checked_artifact_add(cursor: usize, amount: usize, field: &'static str) -> StorageResult<usize> {
    cursor.checked_add(amount).ok_or_else(|| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: {field} overflowed usize; remediation=preserve the vault and inspect the Base source-key corpus before retrying"
            ),
        )
    })
}

fn source_census_artifact_layout(
    referenced: &DerivedSourceReferences,
) -> StorageResult<SourceCensusArtifactLayout> {
    if referenced.len() > CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAX_CFS {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CF_BOUND: source column families={} max={}; remediation=inspect unexpected source_cf metadata before retrying",
                referenced.len(),
                CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAX_CFS
            ),
        ));
    }
    let directory_bytes = referenced
        .len()
        .checked_mul(CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                CALYX_GC_CF,
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: directory length overflowed usize",
            )
        })?;
    let mut cursor = checked_artifact_add(
        CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES,
        directory_bytes,
        "directory end",
    )?;
    let mut name_offsets = BTreeMap::new();
    for cf_name in referenced.keys() {
        if cf_name.is_empty() {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CF_EMPTY: derived source reference names an empty column family",
            ));
        }
        let name_offset = cursor;
        cursor = checked_artifact_add(cursor, cf_name.len(), "column-family name end")?;
        name_offsets.insert(cf_name.clone(), name_offset);
    }
    let mut sections = Vec::with_capacity(referenced.len());
    let mut rows = 0_u64;
    for (cf_name, keys) in referenced {
        let data_len = keys.packed_bytes();
        if data_len > u32::MAX as usize {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_SECTION_TOO_LARGE: cf={cf_name} key_bytes={data_len} max={}; remediation=split the source namespace at the schema boundary before retrying",
                    u32::MAX
                ),
            ));
        }
        let range_bytes = keys
            .len()
            .checked_mul(CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES)
            .ok_or_else(|| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: cf={cf_name} range bytes overflowed usize"
                    ),
                )
            })?;
        let ranges_offset = cursor;
        cursor = checked_artifact_add(cursor, range_bytes, "range directory end")?;
        let data_offset = cursor;
        cursor = checked_artifact_add(cursor, data_len, "key data end")?;
        rows = rows
            .checked_add(calyx_len_to_u64(
                CALYX_GC_CF,
                "Calyx GC source-census artifact rows",
                keys.len(),
            )?)
            .ok_or_else(|| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: row count overflowed u64",
                )
            })?;
        sections.push(SourceCensusArtifactSectionLayout {
            cf_name: cf_name.clone(),
            name_offset: name_offsets[cf_name],
            rows: keys.len(),
            ranges_offset,
            data_offset,
            data_len,
        });
    }
    Ok(SourceCensusArtifactLayout {
        sections,
        rows,
        file_len: cursor,
    })
}

fn write_hashed_artifact_bytes(
    file: &mut fs::File,
    hasher: &mut Sha256,
    bytes: &[u8],
) -> std::io::Result<()> {
    file.write_all(bytes)?;
    hasher.update(bytes);
    Ok(())
}

fn encode_source_census_artifact_header(
    pinned_seq: u64,
    base_last_commit_seq: u64,
    base_out_of_band_epoch: u64,
    vault_id_sha256: &[u8; 32],
    layout: &SourceCensusArtifactLayout,
    payload_sha256: &[u8; 32],
) -> StorageResult<[u8; CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES]> {
    let mut header = [0_u8; CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES];
    header[0..16].copy_from_slice(CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAGIC);
    header[16..20].copy_from_slice(&CALYX_GC_SOURCE_CENSUS_ARTIFACT_VERSION.to_le_bytes());
    header[20..24].copy_from_slice(
        &u32::try_from(CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES)
            .map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census artifact header length exceeds u32",
                )
            })?
            .to_le_bytes(),
    );
    header[24..32].copy_from_slice(&pinned_seq.to_le_bytes());
    header[32..40].copy_from_slice(&base_last_commit_seq.to_le_bytes());
    header[40..48].copy_from_slice(&layout.rows.to_le_bytes());
    header[48..52].copy_from_slice(
        &u32::try_from(layout.sections.len())
            .map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census artifact column-family count exceeds u32",
                )
            })?
            .to_le_bytes(),
    );
    header[52..56].copy_from_slice(
        &u32::try_from(CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES)
            .map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census artifact directory width exceeds u32",
                )
            })?
            .to_le_bytes(),
    );
    header[56..60].copy_from_slice(
        &u32::try_from(CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES)
            .map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census artifact range width exceeds u32",
                )
            })?
            .to_le_bytes(),
    );
    header[64..72].copy_from_slice(
        &u64::try_from(layout.file_len)
            .map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census artifact file length exceeds u64",
                )
            })?
            .to_le_bytes(),
    );
    header[72..104].copy_from_slice(payload_sha256);
    header[104..112].copy_from_slice(&base_out_of_band_epoch.to_le_bytes());
    header[112..144].copy_from_slice(vault_id_sha256);
    let header_sha256: [u8; 32] = Sha256::digest(header).into();
    header[144..176].copy_from_slice(&header_sha256);
    Ok(header)
}

#[expect(
    clippy::too_many_lines,
    reason = "stream, release, map, pointer publication, and physical readback form one unambiguous commit boundary"
)]
fn publish_source_census_artifact(
    vault: &SynapseCalyxVault,
    pinned_seq: u64,
    base_last_commit_seq: u64,
    base_out_of_band_epoch: u64,
    referenced: DerivedSourceReferences,
) -> StorageResult<MappedDerivedSourceReferences> {
    let layout = source_census_artifact_layout(&referenced)?;
    let artifact_id = CALYX_GC_SOURCE_CENSUS_NEXT_ARTIFACT_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = format!(
        "{CALYX_GC_SOURCE_CENSUS_ARTIFACT_PREFIX}{pinned_seq:020}-{:010}-{artifact_id:020}{CALYX_GC_SOURCE_CENSUS_ARTIFACT_SUFFIX}",
        std::process::id()
    );
    let artifact_dir = source_census_artifact_dir(vault);
    let artifact_path = artifact_dir.join(&file_name);
    let vault_id_sha256 = source_census_vault_id_sha256(vault);
    let zero_header = [0_u8; CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES];
    let mut completed_payload_sha256 = None;
    durable_artifact::publish_immutable_with(
        &artifact_path,
        CALYX_GC_SOURCE_CENSUS_ARTIFACT_LABEL,
        |file| {
            file.write_all(&zero_header)?;
            let mut hasher = Sha256::new();
            for section in &layout.sections {
                let mut directory = [0_u8; CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES];
                let cf_name_len = u32::try_from(section.cf_name.len()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "source-census column-family name length exceeds u32 for {}",
                            section.cf_name
                        ),
                    )
                })?;
                directory[0..8].copy_from_slice(&(section.name_offset as u64).to_le_bytes());
                directory[8..12].copy_from_slice(&cf_name_len.to_le_bytes());
                directory[16..24].copy_from_slice(&(section.rows as u64).to_le_bytes());
                directory[24..32].copy_from_slice(&(section.ranges_offset as u64).to_le_bytes());
                directory[32..40].copy_from_slice(&(section.data_offset as u64).to_le_bytes());
                directory[40..48].copy_from_slice(&(section.data_len as u64).to_le_bytes());
                write_hashed_artifact_bytes(file, &mut hasher, &directory)?;
            }
            for section in &layout.sections {
                write_hashed_artifact_bytes(file, &mut hasher, section.cf_name.as_bytes())?;
            }
            for section in &layout.sections {
                let keys = &referenced[&section.cf_name];
                let mut data_offset = 0_u32;
                for range in &keys.ranges {
                    let key = packed_source_reference_key(&keys.chunks, *range);
                    let key_len = u16::try_from(key.len()).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "source-census key length {} exceeds u16 for {}",
                                key.len(),
                                section.cf_name
                            ),
                        )
                    })?;
                    let mut encoded = [0_u8; CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES];
                    encoded[0..4].copy_from_slice(&data_offset.to_le_bytes());
                    encoded[4..6].copy_from_slice(&key_len.to_le_bytes());
                    write_hashed_artifact_bytes(file, &mut hasher, &encoded)?;
                    data_offset = data_offset.checked_add(u32::from(key_len)).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "source-census key offset overflowed u32 for {}",
                                section.cf_name
                            ),
                        )
                    })?;
                }
                let planned_data_len = u32::try_from(section.data_len).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "source-census planned key bytes exceed u32 for {}",
                            section.cf_name
                        ),
                    )
                })?;
                if data_offset != planned_data_len {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "source-census planned key bytes {} differ from encoded {} for {}",
                            section.data_len, data_offset, section.cf_name
                        ),
                    ));
                }
                for range in &keys.ranges {
                    write_hashed_artifact_bytes(
                        file,
                        &mut hasher,
                        packed_source_reference_key(&keys.chunks, *range),
                    )?;
                }
            }
            let payload_sha256: [u8; 32] = hasher.finalize().into();
            let header = encode_source_census_artifact_header(
                pinned_seq,
                base_last_commit_seq,
                base_out_of_band_epoch,
                &vault_id_sha256,
                &layout,
                &payload_sha256,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&header)?;
            completed_payload_sha256 = Some(payload_sha256);
            Ok(())
        },
    )
    .map_err(|source| {
        let source = SynapseCalyxError::from_calyx(
            "publish immutable mapped source-census generation",
            &source,
        );
        calyx_write_failed(
            CALYX_GC_CF,
            "publish immutable mapped source-census generation",
            &source,
        )
    })?;
    if completed_payload_sha256.is_none() {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_WRITE_INCOMPLETE: durable publisher returned without the writer completing",
        ));
    }
    let heap_metrics = derived_source_reference_metrics(&referenced)?;
    drop(referenced);
    let release = synapse_calyx::release_process_memory("storage_gc_source_census_mapped")
        .map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "release heap source census before validating its mapped generation",
                &source,
            )
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_GC_SOURCE_CENSUS_HEAP_RELEASED",
        baseline_rows = heap_metrics.rows,
        packed_key_capacity_bytes = heap_metrics.key_capacity_bytes,
        range_index_capacity_bytes = heap_metrics.range_capacity_bytes,
        private_bytes_before = release.private_bytes_before,
        private_bytes_after = release.private_bytes_after,
        private_bytes_reclaimed = release.private_bytes_reclaimed,
        release_elapsed_us = release.elapsed_us,
        "destroyed and released the corpus-sized heap census before touching the read-only mapping"
    );
    let mapped = open_source_census_artifact(&artifact_path, &vault_id_sha256)?;
    durable_artifact::publish_current(
        &source_census_current_path(vault),
        format!("{file_name}\n").as_bytes(),
        CALYX_GC_SOURCE_CENSUS_ARTIFACT_LABEL,
    )
    .map_err(|source| {
        let source =
            SynapseCalyxError::from_calyx("publish current mapped source-census pointer", &source);
        calyx_write_failed(
            CALYX_GC_CF,
            "publish current mapped source-census pointer",
            &source,
        )
    })?;
    let current_readback = fs::read_to_string(source_census_current_path(vault)).map_err(|source| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_CURRENT_READBACK_FAILED: read CURRENT after publish failed: {source}"
            ),
        )
    })?;
    if current_readback != format!("{file_name}\n") {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_CURRENT_READBACK_MISMATCH: expected {file_name:?}, got {:?}",
                current_readback.trim_end()
            ),
        ));
    }
    cleanup_obsolete_source_census_artifacts(&artifact_dir, &file_name);
    tracing::info!(
        code = "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_PUBLISHED",
        path = %artifact_path.display(),
        pinned_seq,
        base_last_commit_seq,
        base_out_of_band_epoch,
        rows = layout.rows,
        column_families = layout.sections.len(),
        file_bytes = layout.file_len,
        "published and independently reopened the immutable mapped source-census generation"
    );
    Ok(mapped)
}

fn cleanup_obsolete_source_census_artifacts(artifact_dir: &Path, current_name: &str) {
    let entries = match fs::read_dir(artifact_dir) {
        Ok(entries) => entries,
        Err(source) => {
            tracing::error!(
                code = "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CLEANUP_READ_FAILED",
                path = %artifact_dir.display(),
                error = %source,
                "could not enumerate obsolete source-census generations after a successful publication"
            );
            return;
        }
    };
    let temp_prefix = format!(".{CALYX_GC_SOURCE_CENSUS_ARTIFACT_PREFIX}");
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                tracing::error!(
                    code = "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CLEANUP_ENTRY_FAILED",
                    path = %artifact_dir.display(),
                    error = %source,
                    "could not inspect one obsolete source-census generation"
                );
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let generation = name.starts_with(CALYX_GC_SOURCE_CENSUS_ARTIFACT_PREFIX)
            && name.ends_with(CALYX_GC_SOURCE_CENSUS_ARTIFACT_SUFFIX);
        let unpublished_temp = name.starts_with(&temp_prefix)
            && Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"));
        if name == current_name || (!generation && !unpublished_temp) {
            continue;
        }
        let path = entry.path();
        if let Err(source) =
            durable_artifact::remove_obsolete(&path, CALYX_GC_SOURCE_CENSUS_ARTIFACT_LABEL)
        {
            tracing::error!(
                code = "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CLEANUP_REMOVE_FAILED",
                path = %path.display(),
                error_code = source.code,
                error = %source.message,
                "could not remove one obsolete source-census generation; the current pointer remains authoritative and the next publication will retry cleanup"
            );
        }
    }
}

fn validate_artifact_range(
    file_len: usize,
    offset: u64,
    len: u64,
    field: &str,
) -> StorageResult<(usize, usize)> {
    let offset = usize::try_from(offset).map_err(|_| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            format!("STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: {field} offset exceeds usize"),
        )
    })?;
    let len = usize::try_from(len).map_err(|_| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            format!("STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: {field} length exceeds usize"),
        )
    })?;
    let end = checked_artifact_add(offset, len, "validated mapped range")?;
    if end > file_len {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_BOUNDS: {field} offset={offset} len={len} exceeds file_len={file_len}"
            ),
        ));
    }
    Ok((offset, len))
}

#[expect(
    clippy::too_many_lines,
    reason = "the immutable format is validated in one linear fail-closed decoder"
)]
fn open_source_census_artifact(
    path: &Path,
    expected_vault_id_sha256: &[u8; 32],
) -> StorageResult<MappedDerivedSourceReferences> {
    let artifact = MmapColumn::open(path).map_err(|source| {
        let source = SynapseCalyxError::from_calyx("memory-map source-census generation", &source);
        calyx_write_failed(CALYX_GC_CF, "memory-map source-census generation", &source)
    })?;
    let bytes = artifact.as_bytes();
    if bytes.len() < CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES
        || bytes.get(0..16) != Some(CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAGIC.as_slice())
        || read_artifact_u32(bytes, 16) != CALYX_GC_SOURCE_CENSUS_ARTIFACT_VERSION
        || read_artifact_u32(bytes, 20) != CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES_U32
        || read_artifact_u32(bytes, 52) != CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES_U32
        || read_artifact_u32(bytes, 56) != CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES_U32
        || !bytes
            .get(60..64)
            .is_some_and(|reserved| reserved.iter().all(|byte| *byte == 0))
        || !bytes
            .get(176..CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES)
            .is_some_and(|reserved| reserved.iter().all(|byte| *byte == 0))
    {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_INVALID: {} has an unknown or truncated format; remediation=preserve the file and rebuild the derived census from authoritative Base rows",
                path.display()
            ),
        ));
    }
    let mut header_for_hash = [0_u8; CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES];
    header_for_hash.copy_from_slice(&bytes[..CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES]);
    header_for_hash[144..176].fill(0);
    let actual_header_sha256 = Sha256::digest(header_for_hash);
    let expected_header_sha256 = &bytes[144..176];
    if expected_header_sha256 != actual_header_sha256.as_slice() {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_HASH_MISMATCH: path={} expected={} actual={}; remediation=preserve the corrupt artifact and rebuild from authoritative Base rows",
                path.display(),
                constellations::hex_encode(expected_header_sha256),
                constellations::hex_encode(&actual_header_sha256)
            ),
        ));
    }
    let artifact_vault_id_sha256 = &bytes[112..144];
    if artifact_vault_id_sha256 != expected_vault_id_sha256 {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_VAULT_MISMATCH: path={} artifact_vault_id_sha256={} live_vault_id_sha256={}; remediation=preserve the foreign generation and rebuild from this vault's authoritative Base rows",
                path.display(),
                constellations::hex_encode(artifact_vault_id_sha256),
                constellations::hex_encode(expected_vault_id_sha256)
            ),
        ));
    }
    let declared_file_len = usize::try_from(read_artifact_u64(bytes, 64)).map_err(|_| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            "source-census declared file length exceeds usize",
        )
    })?;
    if declared_file_len != bytes.len() {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_LENGTH_MISMATCH: path={} declared={} actual={}",
                path.display(),
                declared_file_len,
                bytes.len()
            ),
        ));
    }
    let expected_payload_sha256 = bytes.get(72..104).ok_or_else(|| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            "source-census payload digest is outside the mapped header",
        )
    })?;
    let actual_payload_sha256 = Sha256::digest(
        bytes
            .get(CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES..)
            .ok_or_else(|| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census payload begins outside the mapped file",
                )
            })?,
    );
    if expected_payload_sha256 != actual_payload_sha256.as_slice() {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_HASH_MISMATCH: path={} expected={} actual={}; remediation=preserve the corrupt artifact and rebuild from authoritative Base rows",
                path.display(),
                constellations::hex_encode(expected_payload_sha256),
                constellations::hex_encode(&actual_payload_sha256)
            ),
        ));
    }
    let pinned_seq = read_artifact_u64(bytes, 24);
    let base_last_commit_seq = read_artifact_u64(bytes, 32);
    let base_out_of_band_epoch = read_artifact_u64(bytes, 104);
    if base_last_commit_seq > pinned_seq {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_WATERMARK_INVALID: path={} base_last_commit_seq={} pinned_seq={}",
                path.display(),
                base_last_commit_seq,
                pinned_seq
            ),
        ));
    }
    let declared_rows = read_artifact_u64(bytes, 40);
    let cf_count = usize::try_from(read_artifact_u32(bytes, 48)).map_err(|_| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            "source-census column-family count exceeds usize",
        )
    })?;
    if cf_count > CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAX_CFS {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CF_BOUND: path={} column_families={} max={}",
                path.display(),
                cf_count,
                CALYX_GC_SOURCE_CENSUS_ARTIFACT_MAX_CFS
            ),
        ));
    }
    let directory_len = cf_count
        .checked_mul(CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                CALYX_GC_CF,
                "source-census mapped directory length overflowed usize",
            )
        })?;
    validate_artifact_range(
        bytes.len(),
        CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES as u64,
        directory_len as u64,
        "column-family directory",
    )?;
    let names_offset = checked_artifact_add(
        CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES,
        directory_len,
        "mapped column-family names offset",
    )?;
    let mut total_name_bytes = 0_usize;
    for index in 0..cf_count {
        let directory_offset = CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES
            + (index * CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES);
        total_name_bytes = checked_artifact_add(
            total_name_bytes,
            usize::try_from(read_artifact_u32(bytes, directory_offset + 8)).map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census column-family name length exceeds usize",
                )
            })?,
            "mapped column-family names length",
        )?;
    }
    let mut expected_name_offset = names_offset;
    let mut expected_section_offset =
        checked_artifact_add(names_offset, total_name_bytes, "mapped section start")?;
    let mut sections = BTreeMap::new();
    let mut rows = 0_u64;
    let mut key_bytes = 0_u64;
    let mut previous_cf_name: Option<String> = None;
    for index in 0..cf_count {
        let directory_offset = CALYX_GC_SOURCE_CENSUS_ARTIFACT_HEADER_BYTES
            + (index * CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_BYTES);
        if !bytes
            .get(directory_offset + 12..directory_offset + 16)
            .is_some_and(|reserved| reserved.iter().all(|byte| *byte == 0))
        {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_DIRECTORY_RESERVED: path={} index={} contains nonzero reserved bytes",
                    path.display(),
                    index
                ),
            ));
        }
        let (name_offset, name_len) = validate_artifact_range(
            bytes.len(),
            read_artifact_u64(bytes, directory_offset),
            u64::from(read_artifact_u32(bytes, directory_offset + 8)),
            "column-family name",
        )?;
        if name_offset != expected_name_offset {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_LAYOUT: path={} index={} name_offset={} expected={expected_name_offset}",
                    path.display(),
                    index,
                    name_offset
                ),
            ));
        }
        expected_name_offset =
            checked_artifact_add(expected_name_offset, name_len, "next mapped name offset")?;
        let cf_name = std::str::from_utf8(&bytes[name_offset..name_offset + name_len])
            .map_err(|source| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CF_UTF8: path={} index={} error={source}",
                        path.display(),
                        index
                    ),
                )
            })?
            .to_owned();
        if cf_name.is_empty()
            || previous_cf_name
                .as_ref()
                .is_some_and(|previous| previous >= &cf_name)
        {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_CF_ORDER: path={} index={} cf={cf_name:?}",
                    path.display(),
                    index
                ),
            ));
        }
        previous_cf_name = Some(cf_name.clone());
        let section_rows_u64 = read_artifact_u64(bytes, directory_offset + 16);
        let section_rows = usize::try_from(section_rows_u64).map_err(|_| {
            calyx_write_failed_detail(
                CALYX_GC_CF,
                format!("source-census row count exceeds usize for {cf_name}"),
            )
        })?;
        let ranges_len = section_rows
            .checked_mul(CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES)
            .ok_or_else(|| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!("source-census range length overflowed for {cf_name}"),
                )
            })?;
        let (ranges_offset, _) = validate_artifact_range(
            bytes.len(),
            read_artifact_u64(bytes, directory_offset + 24),
            ranges_len as u64,
            "source-key ranges",
        )?;
        if ranges_offset != expected_section_offset {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_LAYOUT: path={} cf={} ranges_offset={} expected={expected_section_offset}",
                    path.display(),
                    cf_name,
                    ranges_offset
                ),
            ));
        }
        let (data_offset, data_len) = validate_artifact_range(
            bytes.len(),
            read_artifact_u64(bytes, directory_offset + 32),
            read_artifact_u64(bytes, directory_offset + 40),
            "source-key bytes",
        )?;
        let expected_data_offset =
            checked_artifact_add(ranges_offset, ranges_len, "mapped key data offset")?;
        if data_offset != expected_data_offset {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_LAYOUT: path={} cf={} data_offset={} expected={expected_data_offset}",
                    path.display(),
                    cf_name,
                    data_offset
                ),
            ));
        }
        expected_section_offset =
            checked_artifact_add(data_offset, data_len, "next mapped section offset")?;
        let mut previous_key: Option<&[u8]> = None;
        let mut expected_key_offset = 0_usize;
        for row in 0..section_rows {
            let range_offset = ranges_offset + (row * CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES);
            let key_offset =
                usize::try_from(read_artifact_u32(bytes, range_offset)).map_err(|_| {
                    calyx_write_failed_detail(
                        CALYX_GC_CF,
                        format!("source-census key offset exceeds usize for {cf_name}"),
                    )
                })?;
            let key_len = usize::from(read_artifact_u16(bytes, range_offset + 4));
            let reserved_zero = bytes
                .get(range_offset + 6..range_offset + 8)
                .is_some_and(|reserved| reserved.iter().all(|byte| *byte == 0));
            if key_len == 0 || key_offset != expected_key_offset || !reserved_zero {
                return Err(calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_INVALID: path={} cf={} row={} key_offset={} expected_offset={} key_len={} reserved_zero={reserved_zero}",
                        path.display(),
                        cf_name,
                        row,
                        key_offset,
                        expected_key_offset,
                        key_len
                    ),
                ));
            }
            let key_end = checked_artifact_add(key_offset, key_len, "mapped key end")?;
            if key_end > data_len {
                return Err(calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_INVALID: path={} cf={} row={} key_end={} data_len={}",
                        path.display(),
                        cf_name,
                        row,
                        key_end,
                        data_len
                    ),
                ));
            }
            let key = &bytes[data_offset + key_offset..data_offset + key_end];
            if previous_key.is_some_and(|previous| previous >= key) {
                return Err(calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_KEY_ORDER: path={} cf={} row={}",
                        path.display(),
                        cf_name,
                        row
                    ),
                ));
            }
            previous_key = Some(key);
            expected_key_offset = key_end;
        }
        if expected_key_offset != data_len {
            return Err(calyx_write_failed_detail(
                CALYX_GC_CF,
                format!(
                    "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_DATA_LENGTH: path={} cf={} indexed={} data_len={}",
                    path.display(),
                    cf_name,
                    expected_key_offset,
                    data_len
                ),
            ));
        }
        rows = rows.checked_add(section_rows_u64).ok_or_else(|| {
            calyx_write_failed_detail(CALYX_GC_CF, "source-census mapped rows overflowed u64")
        })?;
        key_bytes = key_bytes
            .checked_add(u64::try_from(data_len).map_err(|_| {
                calyx_write_failed_detail(CALYX_GC_CF, "source-census mapped key bytes exceed u64")
            })?)
            .ok_or_else(|| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "source-census mapped key bytes overflowed u64",
                )
            })?;
        sections.insert(
            cf_name,
            MappedSourceReferenceSection {
                rows: section_rows,
                ranges_offset,
                data_offset,
            },
        );
    }
    if rows != declared_rows {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_ROW_COUNT: path={} declared={} decoded={rows}",
                path.display(),
                declared_rows
            ),
        ));
    }
    if expected_section_offset != bytes.len() {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_LAYOUT: path={} decoded_end={} file_len={}",
                path.display(),
                expected_section_offset,
                bytes.len()
            ),
        ));
    }
    let range_bytes = rows
        .checked_mul(CALYX_GC_SOURCE_CENSUS_ARTIFACT_RANGE_BYTES as u64)
        .ok_or_else(|| {
            calyx_write_failed_detail(CALYX_GC_CF, "mapped source-census range bytes overflowed")
        })?;
    Ok(MappedDerivedSourceReferences {
        artifact,
        pinned_seq,
        base_last_commit_seq,
        base_out_of_band_epoch,
        sections,
        metrics: DerivedSourceReferenceMetrics {
            rows,
            key_bytes,
            key_capacity_bytes: key_bytes,
            chunks: u64::try_from(cf_count).map_err(|_| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "mapped source-census section count exceeds u64",
                )
            })?,
            range_bytes,
            range_capacity_bytes: range_bytes,
        },
    })
}

fn read_current_source_census_artifact(
    vault: &SynapseCalyxVault,
) -> StorageResult<Option<MappedDerivedSourceReferences>> {
    let current_path = source_census_current_path(vault);
    if !current_path.exists() {
        return Ok(None);
    }
    let pointer_file = fs::read_to_string(&current_path).map_err(|source| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_CURRENT_READ_FAILED: path={} error={source}",
                current_path.display()
            ),
        )
    })?;
    let Some(pointer) = pointer_file.strip_suffix('\n') else {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_CURRENT_INVALID: path={} pointer={pointer_file:?}; remediation=preserve CURRENT and its generation, then rebuild from authoritative Base rows",
                current_path.display()
            ),
        ));
    };
    if pointer.is_empty()
        || pointer.len() > 255
        || pointer.contains(['\r', '\n'])
        || !pointer.starts_with(CALYX_GC_SOURCE_CENSUS_ARTIFACT_PREFIX)
        || !pointer.ends_with(CALYX_GC_SOURCE_CENSUS_ARTIFACT_SUFFIX)
        || Path::new(pointer)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(pointer)
    {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_CURRENT_INVALID: path={} pointer={pointer:?}; remediation=preserve CURRENT and its generation, then rebuild from authoritative Base rows",
                current_path.display()
            ),
        ));
    }
    let vault_id_sha256 = source_census_vault_id_sha256(vault);
    open_source_census_artifact(
        &source_census_artifact_dir(vault).join(pointer),
        &vault_id_sha256,
    )
    .map(Some)
}

/// Lease lifetime for the pinned `Base` census snapshot (#2058).
///
/// Deliberately [`crate::COHERENT_SCAN_MAX_AGE_MS`] — the crate's already
/// declared ceiling for how long any storage reader may hold one pinned Calyx
/// MVCC view — rather than a second, independently guessed number. This is a
/// **liveness bound on the pin, not a timeout on the walk**: the walk still
/// runs at whatever speed the corpus dictates (13.5 s over 3,671 pages on the
/// live vault), and the lease only decides how long the vault's snapshot GC is
/// obliged to keep the pinned versions reclaimable-but-retained before the
/// watchdog aborts the reader. A census that somehow ran past it fails loud on
/// its next page read (`ensure_snapshot_live`) instead of silently reading a
/// view whose versions have started being reclaimed.
const CALYX_GC_SOURCE_CENSUS_LEASE_MS: u64 = crate::COHERENT_SCAN_MAX_AGE_MS;

/// A reader lease held for exactly as long as one bounded multi-page read, and
/// released on **every** exit path including an unwind (#2058).
///
/// `SynapseCalyxVault::pin_reader` registers the lease in the vault's
/// oldest-pinned-seq accounting, which is what makes snapshot version GC keep
/// the pinned view alive — and equally what makes a leaked lease pin the vault's
/// GC safe point forever. `Drop` is therefore where the release lives, mirroring
/// calyx-aster's own `ScopedSnapshot` idiom rather than trusting every `?` in
/// the walk to route through an explicit release.
struct CalyxPinnedReader<'vault> {
    vault: &'vault SynapseCalyxVault,
    snapshot: calyx_aster::mvcc::Snapshot,
    cf_name: &'static str,
    site: &'static str,
}

impl<'vault> CalyxPinnedReader<'vault> {
    /// Pins the current committed sequence for the whole read.
    ///
    /// Pins **now**, at latest, rather than re-pinning a sequence observed
    /// earlier: a sequence already in the past may have had versions reclaimed
    /// before the pin registered, and pinning it then would silently describe a
    /// view the store no longer holds in full.
    fn pin(
        vault: &'vault SynapseCalyxVault,
        cf_name: &'static str,
        site: &'static str,
        max_age_ms: u64,
    ) -> StorageResult<Self> {
        let snapshot = vault
            .pin_reader(Freshness::FreshDerived, max_age_ms)
            .map_err(|source| {
                calyx_write_failed(
                    cf_name,
                    "pin one committed Calyx MVCC sequence for a bounded census",
                    &source,
                )
            })?;
        tracing::debug!(
            code = "STORAGE_CALYX_CENSUS_PINNED",
            site,
            cf = cf_name,
            lease_id = snapshot.lease().id(),
            pinned_seq = snapshot.seq(),
            issued_at_unix_ms = snapshot.lease().issued_at(),
            expires_at_unix_ms = snapshot.lease().expires_at(),
            "pinned one committed Calyx sequence for a bounded census"
        );
        Ok(Self {
            vault,
            snapshot,
            cf_name,
            site,
        })
    }

    const fn snapshot(&self) -> calyx_aster::mvcc::Snapshot {
        self.snapshot
    }

    const fn pinned_seq(&self) -> u64 {
        self.snapshot.seq()
    }
}

impl Drop for CalyxPinnedReader<'_> {
    fn drop(&mut self) {
        let lease_id = self.snapshot.lease().id();
        if self.vault.release_reader(lease_id) {
            tracing::debug!(
                code = "STORAGE_CALYX_CENSUS_UNPINNED",
                site = self.site,
                cf = self.cf_name,
                lease_id,
                pinned_seq = self.snapshot.seq(),
                "released the pinned Calyx census sequence"
            );
            return;
        }
        // The lease was already gone: the watchdog aborted it for exceeding
        // `max_age_ms`. Every page read after that point failed closed, so this
        // cannot have produced a wrong census — but it is the exact evidence a
        // slow census leaves behind, so it is recorded rather than swallowed.
        tracing::warn!(
            code = "STORAGE_CALYX_CENSUS_LEASE_EXPIRED",
            site = self.site,
            cf = self.cf_name,
            lease_id,
            pinned_seq = self.snapshot.seq(),
            "pinned Calyx census lease was no longer registered at release; the reader watchdog \
             had already aborted it for exceeding its bounded lifetime"
        );
    }
}

/// What one pinned-sequence walk observed. Provenance of the census, reported
/// rather than assumed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CalyxPinnedCfWalk {
    pinned_seq: u64,
    pages: usize,
    rows_examined: usize,
    rows_visited: usize,
}

/// Folds one whole column family page by page **at one pinned MVCC sequence**,
/// releasing the row-table read guard between pages (#2058).
///
/// This is the difference between a census and a race. `walk_cf_latest` opens a
/// *new latest* view per page, so its output only describes one instant when no
/// commit lands during the entire scan — on the live vault that is 3,671 pages
/// and 13.5 s of continuous-write exposure, and
/// `SynapseCalyxCfWalk::atomic()` was false on every attempt. Pinning instead
/// makes every page read the same sequence by construction: the lease enters
/// the vault's oldest-pinned-seq accounting, snapshot version GC clamps its
/// safe point to it, and `reclaim_chain` retains each key's boundary version at
/// or below that safe point — so no compaction can retire a version this walk
/// still needs, no matter how long it runs or how many commits land.
///
/// The guard discipline of #2041 is unchanged and deliberately so: one page,
/// one bounded acquisition of the vault-wide row-table read guard every
/// constellation writer needs, never one hold across the corpus.
///
/// # Errors
///
/// Fails closed — never partially — when the pin cannot serve a page (an
/// expired lease, a blocked row, an unreadable serving view), when a page
/// resolves a sequence other than the pinned one, when a page reports more rows
/// without a resume cursor, or when a cursor fails to advance. The visitor's own
/// error is propagated verbatim.
fn walk_cf_pages_pinned<V>(
    reader: &CalyxPinnedReader<'_>,
    cf: ColumnFamily,
    visit: V,
) -> StorageResult<CalyxPinnedCfWalk>
where
    V: FnMut(&[u8], &[u8]) -> StorageResult<()>,
{
    walk_cf_range_pages_pinned(reader, cf, &KeyRange::all(), visit)
}

/// Range-scoped form of [`walk_cf_pages_pinned`], used when multiple logical
/// Synapse namespaces in the physical KV family must share the same pinned
/// sequence with a later non-KV census.
fn walk_cf_range_pages_pinned<V>(
    reader: &CalyxPinnedReader<'_>,
    cf: ColumnFamily,
    range: &KeyRange,
    mut visit: V,
) -> StorageResult<CalyxPinnedCfWalk>
where
    V: FnMut(&[u8], &[u8]) -> StorageResult<()>,
{
    let started = Instant::now();
    let page_rows = if cf == ColumnFamily::Base {
        SYNAPSE_CALYX_BASE_CF_WALK_PAGE_ROWS
    } else {
        CALYX_INSPECT_SWEEP_PAGE_ROWS
    };
    let mut visitor_error = None;
    let stream_result = reader.vault.walk_cf_range_snapshot(
        reader.snapshot(),
        cf,
        range,
        page_rows,
        |key, value| match visit(key, value) {
            Ok(()) => Ok(SynapseCalyxWalkStep::Continue),
            Err(error) => {
                visitor_error = Some(error);
                Ok(SynapseCalyxWalkStep::Stop)
            }
        },
    );
    let streamed = match (stream_result, visitor_error) {
        (Ok(_), Some(error)) => return Err(error),
        (Err(stream_error), Some(visitor_error)) => {
            return Err(calyx_write_failed_detail(
                reader.cf_name,
                format!(
                    "CALYX_PINNED_CENSUS_STREAM_AND_VISITOR_FAILED: the {} {} visitor failed with {visitor_error}; after stopping it, the persistent stream also failed with {stream_error}; remediation=repair both independently reported failures before retrying",
                    cf.name(),
                    reader.site
                ),
            ));
        }
        (Err(source), None) => {
            return Err(calyx_write_failed(
                reader.cf_name,
                &format!(
                    "stream the {} {} census at pinned committed sequence {}",
                    cf.name(),
                    reader.site,
                    reader.pinned_seq()
                ),
                &source,
            ));
        }
        (Ok(walk), None) => walk,
    };
    if !streamed.atomic()
        || streamed.snapshot_seq_first != reader.pinned_seq()
        || streamed.snapshot_seq_last != reader.pinned_seq()
    {
        return Err(calyx_write_failed_detail(
            reader.cf_name,
            format!(
                "CALYX_PINNED_CENSUS_SEQUENCE_DRIFTED: the {} {} stream reported pages={} first_seq={} last_seq={} but the census pinned {}; remediation=repair the snapshot stream so every page is served by the exact registered lease",
                cf.name(),
                reader.site,
                streamed.pages,
                streamed.snapshot_seq_first,
                streamed.snapshot_seq_last,
                reader.pinned_seq()
            ),
        ));
    }
    let walk = CalyxPinnedCfWalk {
        pinned_seq: reader.pinned_seq(),
        pages: streamed.pages,
        rows_examined: streamed.rows_examined,
        rows_visited: streamed.rows_visited,
    };
    tracing::debug!(
        code = "STORAGE_CALYX_PINNED_CENSUS_WALK",
        site = reader.site,
        cf = cf.name(),
        pinned_seq = walk.pinned_seq,
        pages = walk.pages,
        page_rows,
        rows_visited = walk.rows_visited,
        rows_examined = walk.rows_examined,
        elapsed_ms = started.elapsed().as_millis(),
        "folded a column family at one pinned committed sequence through one persistent immutable cursor and released all cursor ownership at completion"
    );
    Ok(walk)
}

/// Indexes every live derived constellation's source reference from **one**
/// pinned committed sequence (#2058, protecting #1882).
///
/// The previous shape asked the vault for quiescence: walk `Base` at latest,
/// require the first and last page to report the same sequence, retry three
/// times, and refuse the whole GC tick otherwise. On a vault taking continuous
/// MCP writes that is not a convergence strategy — it needs 13.5 s of *zero*
/// commits, three times running — and the live daemon's GC therefore never ran
/// and its storage health sat permanently at `error`.
///
/// Pinning replaces the requirement with a guarantee. The lease fixes the
/// vault's snapshot-GC safe point at the pinned sequence, so every page reads
/// exactly the view that existed at that instant regardless of what commits
/// while the walk runs, and writers are never blocked: they allocate later
/// sequences that this census simply cannot see. There is nothing left to
/// retry, so there is no retry.
///
/// **What the pinned instant does and does not promise.** The protection set is
/// exact as of the pinned sequence. A derivation that commits *after* the pin
/// names a source row that already existed at the pin (its `CxId` is
/// content-addressed over that row's bytes), and cap eviction drains
/// oldest-written first, so the row a derivation just measured is the last row
/// in its family eligible for eviction. The same window existed before this
/// change — a census, however atomic, is always taken before the deletions it
/// authorises — and it is bounded here by one GC interval.
#[expect(
    clippy::too_many_lines,
    reason = "one exact baseline/delta/rebase state machine keeps every cache transition and its fail-closed invalidation adjacent"
)]
fn refresh_derived_source_references(
    vault: &SynapseCalyxVault,
    cache: &mut Option<CalyxGcSourceCensusCache>,
) -> StorageResult<gc::DerivedSourceCensus> {
    // Sample before pinning so an out-of-band Base replacement anywhere from
    // this boundary through the complete pinned walk invalidates the rebuild.
    // Ordinary MVCC commits are allowed: the snapshot sequence, not a quiet
    // writer window, defines the exact view.
    let base_signal_before_pin = vault.cf_change_signal(ColumnFamily::Base);
    let reader = CalyxPinnedReader::pin(
        vault,
        CALYX_GC_CF,
        "derived_source_references",
        CALYX_GC_SOURCE_CENSUS_LEASE_MS,
    )?;

    let Some(mut previous) = cache.take() else {
        if let Some(baseline) = read_current_source_census_artifact(vault)? {
            let (live_base_last_commit_seq, live_base_out_of_band_epoch) =
                vault.cf_change_signal(ColumnFamily::Base);
            if live_base_last_commit_seq < baseline.base_last_commit_seq {
                return Err(calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_SEQUENCE_INVERTED: artifact_base_last_commit_seq={} live_base_last_commit_seq={live_base_last_commit_seq}; remediation=repair the vault sequence regression before GC",
                        baseline.base_last_commit_seq
                    ),
                ));
            }
            if reader.pinned_seq() < baseline.pinned_seq {
                return Err(calyx_write_failed_detail(
                    CALYX_GC_CF,
                    format!(
                        "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_FUTURE: artifact_pinned_seq={} reader_pinned_seq={}; remediation=repair the vault/artifact sequence inversion before GC",
                        baseline.pinned_seq,
                        reader.pinned_seq()
                    ),
                ));
            }
            if live_base_last_commit_seq <= baseline.pinned_seq
                && live_base_out_of_band_epoch == baseline.base_out_of_band_epoch
            {
                let artifact_pinned_seq = baseline.pinned_seq;
                let current_name = baseline
                    .artifact
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        calyx_write_failed_detail(
                            CALYX_GC_CF,
                            format!(
                                "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_NAME_INVALID: mapped path {} has no UTF-8 filename",
                                baseline.artifact.path().display()
                            ),
                        )
                    })?
                    .to_owned();
                let index = DerivedSourceReferenceIndex {
                    baseline,
                    delta: BTreeMap::new(),
                };
                let census = derived_source_census_from_index(
                    &index,
                    DerivedSourceCensusProvenance {
                        mode: CALYX_GC_SOURCE_CENSUS_MAPPED_REUSE,
                        pinned_seq: reader.pinned_seq(),
                        previous_pinned_seq: Some(artifact_pinned_seq),
                        pages: 0,
                        base_rows_visited: 0,
                        changed_base_keys: 0,
                        rebase_reason: None,
                    },
                )?;
                emit_derived_source_census(&index, census)?;
                cleanup_obsolete_source_census_artifacts(
                    &source_census_artifact_dir(vault),
                    &current_name,
                );
                *cache = Some(CalyxGcSourceCensusCache {
                    pinned_seq: reader.pinned_seq(),
                    referenced: index,
                });
                return Ok(census);
            }
            tracing::info!(
                code = "STORAGE_CALYX_GC_SOURCE_CENSUS_ARTIFACT_STALE",
                artifact_path = %baseline.artifact.path().display(),
                artifact_pinned_seq = baseline.pinned_seq,
                live_base_last_commit_seq,
                artifact_base_out_of_band_epoch = baseline.base_out_of_band_epoch,
                live_base_out_of_band_epoch,
                "the mapped source census predates a Base commit or physical content transition; rebuilding from the authoritative pinned snapshot"
            );
            drop(baseline);
        }
        let (rebuilt, census) = rebuild_derived_source_references(
            vault,
            &reader,
            CALYX_GC_SOURCE_CENSUS_FULL_BASELINE,
            None,
            0,
            None,
            base_signal_before_pin.1,
        )?;
        *cache = Some(rebuilt);
        return Ok(census);
    };

    let previous_pinned_seq = previous.pinned_seq;
    if reader.pinned_seq() < previous_pinned_seq {
        let error = calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_SEQUENCE_INVERTED: previous_pinned_seq={previous_pinned_seq} current_pinned_seq={}; refusing to apply an inverted reachability delta; remediation=repair the vault sequence regression before retrying",
                reader.pinned_seq()
            ),
        );
        *cache = Some(previous);
        return Err(error);
    }
    if base_signal_before_pin.1 != previous.referenced.baseline.base_out_of_band_epoch {
        tracing::warn!(
            code = "STORAGE_CALYX_GC_SOURCE_CENSUS_OUT_OF_BAND_REBASE_REQUIRED",
            previous_pinned_seq,
            current_pinned_seq = reader.pinned_seq(),
            previous_base_out_of_band_epoch = previous.referenced.baseline.base_out_of_band_epoch,
            current_base_out_of_band_epoch = base_signal_before_pin.1,
            "Base physical content changed outside a commit; rebuilding exact reachability from the current pinned snapshot"
        );
        release_derived_source_references_before_rebase(
            previous,
            CALYX_GC_SOURCE_CENSUS_REBASE_OUT_OF_BAND,
        )?;
        let (rebuilt, census) = rebuild_derived_source_references(
            vault,
            &reader,
            CALYX_GC_SOURCE_CENSUS_FULL_REBASE,
            Some(previous_pinned_seq),
            0,
            Some(CALYX_GC_SOURCE_CENSUS_REBASE_OUT_OF_BAND),
            base_signal_before_pin.1,
        )?;
        *cache = Some(rebuilt);
        return Ok(census);
    }
    let history_floor = vault.changed_key_history_floor();
    if previous_pinned_seq < history_floor {
        // The cache is an optimization over authoritative Base rows, not an
        // authority of its own. Aster deliberately discards pre-recovery
        // per-key history when it restores a latest-only router. Once this
        // process has advanced the history floor past our cached sequence,
        // no delta can prove the interval. Repeating that impossible delta
        // request made every retention-GC pass fail forever and left
        // logically expired rows physically resident.
        //
        // Re-pin already happened above, so rebuild from the exact current
        // snapshot and replace the cache only after the complete baseline
        // succeeds. Other error classes remain hard failures; there is no
        // partial set and no stale-cache fallback.
        tracing::warn!(
            code = "STORAGE_CALYX_GC_SOURCE_CENSUS_HISTORY_GAP_REBASE_REQUIRED",
            previous_pinned_seq,
            current_pinned_seq = reader.pinned_seq(),
            changed_key_history_floor = history_floor,
            "the cached reachability baseline predates retained Base change history; rebuilding from the authoritative pinned snapshot"
        );
        release_derived_source_references_before_rebase(
            previous,
            CALYX_GC_SOURCE_CENSUS_REBASE_HISTORY_GAP,
        )?;
        let (rebuilt, census) = rebuild_derived_source_references(
            vault,
            &reader,
            CALYX_GC_SOURCE_CENSUS_FULL_REBASE,
            Some(previous_pinned_seq),
            0,
            Some(CALYX_GC_SOURCE_CENSUS_REBASE_HISTORY_GAP),
            base_signal_before_pin.1,
        )?;
        *cache = Some(rebuilt);
        return Ok(census);
    }
    let changed_keys = vault
        .changed_cf_keys_after_snapshot(reader.snapshot(), ColumnFamily::Base, previous_pinned_seq)
        .map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "read the exact Base changed-key delta for derived-source reachability",
                &source,
            )
        })?;
    let changed_base_keys = calyx_len_to_u64(
        CALYX_GC_CF,
        "Calyx GC changed Base keys",
        changed_keys.len(),
    )?;
    if changed_keys.is_empty() {
        previous.pinned_seq = reader.pinned_seq();
        let census = derived_source_census_from_index(
            &previous.referenced,
            DerivedSourceCensusProvenance {
                mode: CALYX_GC_SOURCE_CENSUS_UNCHANGED,
                pinned_seq: reader.pinned_seq(),
                previous_pinned_seq: Some(previous_pinned_seq),
                pages: 0,
                base_rows_visited: 0,
                changed_base_keys: 0,
                rebase_reason: None,
            },
        )?;
        emit_derived_source_census(&previous.referenced, census)?;
        *cache = Some(previous);
        return Ok(census);
    }

    for key in &changed_keys {
        let Some(value) = vault
            .read_cf_snapshot(reader.snapshot(), ColumnFamily::Base, key)
            .map_err(|source| {
                calyx_write_failed(
                    CALYX_GC_CF,
                    "read one changed Base row from the pinned reachability snapshot",
                    &source,
                )
            })?
        else {
            release_derived_source_references_before_rebase(
                previous,
                CALYX_GC_SOURCE_CENSUS_REBASE_TOMBSTONE,
            )?;
            let (rebuilt, census) = rebuild_derived_source_references(
                vault,
                &reader,
                CALYX_GC_SOURCE_CENSUS_FULL_REBASE,
                Some(previous_pinned_seq),
                changed_base_keys,
                Some(CALYX_GC_SOURCE_CENSUS_REBASE_TOMBSTONE),
                base_signal_before_pin.1,
            )?;
            *cache = Some(rebuilt);
            return Ok(census);
        };
        if let Some((source_cf, source_key)) =
            decode_derived_source_reference(&value).map_err(|source| {
                calyx_write_failed(
                    CALYX_GC_CF,
                    "decode one changed Base row's derived source reference",
                    &source,
                )
            })?
        {
            // Base identity is content-addressed and its source pointer is
            // immutable across legitimate anchor/frequency rewrites. New rows
            // therefore add reachability; deletion is the only subtractive
            // transition and takes the full-rebase branch above.
            previous.referenced.insert_delta(source_cf, source_key);
        }
    }

    if previous.referenced.delta_reference_count() >= CALYX_GC_SOURCE_CENSUS_MAX_DELTA_REFERENCES {
        release_derived_source_references_before_rebase(
            previous,
            CALYX_GC_SOURCE_CENSUS_REBASE_DELTA_BOUND,
        )?;
        let (rebuilt, census) = rebuild_derived_source_references(
            vault,
            &reader,
            CALYX_GC_SOURCE_CENSUS_FULL_REBASE,
            Some(previous_pinned_seq),
            changed_base_keys,
            Some(CALYX_GC_SOURCE_CENSUS_REBASE_DELTA_BOUND),
            base_signal_before_pin.1,
        )?;
        *cache = Some(rebuilt);
        return Ok(census);
    }

    previous.pinned_seq = reader.pinned_seq();
    let census = derived_source_census_from_index(
        &previous.referenced,
        DerivedSourceCensusProvenance {
            mode: CALYX_GC_SOURCE_CENSUS_INCREMENTAL_DELTA,
            pinned_seq: reader.pinned_seq(),
            previous_pinned_seq: Some(previous_pinned_seq),
            pages: 0,
            base_rows_visited: changed_base_keys,
            changed_base_keys,
            rebase_reason: None,
        },
    )?;
    emit_derived_source_census(&previous.referenced, census)?;
    *cache = Some(previous);
    Ok(census)
}

/// Destroys the superseded corpus-sized census before allocating its
/// replacement. Keeping both exact indexes alive during a rebase doubles the
/// largest GC allocation and can exceed the daemon's whole-process memory
/// doctrine even though only one index is authoritative. A rebuild failure
/// therefore leaves the cache absent (and fails the pass); the next pass starts
/// from a full authoritative baseline rather than falling back to stale state.
fn release_derived_source_references_before_rebase(
    previous: CalyxGcSourceCensusCache,
    reason: &'static str,
) -> StorageResult<()> {
    let previous_pinned_seq = previous.pinned_seq;
    let metrics = previous.referenced.baseline.metrics;
    let delta_reference_rows = previous.referenced.delta_reference_count();
    drop(previous);
    let release = synapse_calyx::release_process_memory("storage_gc_source_census_rebase")
        .map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "release the superseded source census before an exact rebase",
                &source,
            )
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_GC_SOURCE_CENSUS_RELEASED_BEFORE_REBASE",
        reason,
        previous_pinned_seq,
        baseline_rows = metrics.rows,
        packed_key_capacity_bytes = metrics.key_capacity_bytes,
        range_index_capacity_bytes = metrics.range_capacity_bytes,
        delta_reference_rows,
        private_bytes_before = release.private_bytes_before,
        private_bytes_after = release.private_bytes_after,
        private_bytes_reclaimed = release.private_bytes_reclaimed,
        release_elapsed_us = release.elapsed_us,
        "destroyed the superseded exact source census before allocating its replacement"
    );
    Ok(())
}

fn rebuild_derived_source_references(
    vault: &SynapseCalyxVault,
    reader: &CalyxPinnedReader<'_>,
    mode: &'static str,
    previous_pinned_seq: Option<u64>,
    changed_base_keys: u64,
    rebase_reason: Option<&'static str>,
    expected_base_out_of_band_epoch: u64,
) -> StorageResult<(CalyxGcSourceCensusCache, gc::DerivedSourceCensus)> {
    let mut referenced = DerivedSourceReferences::new();
    let walk = walk_cf_pages_pinned(reader, ColumnFamily::Base, |_key, value| {
        collect_derived_source_reference(value, &mut referenced).map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "index one Base row's derived source reference",
                &source,
            )
        })
    })?;
    for (cf_name, keys) in &mut referenced {
        keys.sort_and_dedup(cf_name).map_err(|detail| {
            calyx_write_failed_detail(
                CALYX_GC_CF,
                format!("STORAGE_CALYX_GC_SOURCE_CENSUS_COMPACTION_FAILED: {detail}"),
            )
        })?;
    }
    let (live_base_last_commit_seq, live_base_out_of_band_epoch) =
        vault.cf_change_signal(ColumnFamily::Base);
    if live_base_out_of_band_epoch != expected_base_out_of_band_epoch {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_OUT_OF_BAND_CHANGE: pinned_seq={} expected_base_out_of_band_epoch={expected_base_out_of_band_epoch} live_base_out_of_band_epoch={live_base_out_of_band_epoch}; refusing to publish across a physical Base replacement; remediation=allow the active Base compaction/retirement to complete before retrying GC",
                walk.pinned_seq,
            ),
        ));
    }
    // A writer may publish a later Base signal while this pinned walk is in
    // progress. That is expected and cannot enter the pinned snapshot. Clamp
    // the diagnostic watermark to the served sequence instead of reintroducing
    // the impossible whole-walk quiescence requirement.
    let base_last_commit_seq = live_base_last_commit_seq.min(walk.pinned_seq);
    let baseline = publish_source_census_artifact(
        vault,
        walk.pinned_seq,
        base_last_commit_seq,
        live_base_out_of_band_epoch,
        referenced,
    )?;
    let base_out_of_band_epoch_after_publish = vault.cf_change_signal(ColumnFamily::Base).1;
    if base_out_of_band_epoch_after_publish != expected_base_out_of_band_epoch {
        return Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_OUT_OF_BAND_CHANGE: pinned_seq={} expected_base_out_of_band_epoch={expected_base_out_of_band_epoch} post_publish_base_out_of_band_epoch={base_out_of_band_epoch_after_publish}; the published generation is stale and this GC pass will not adjudicate deletion; remediation=allow the active Base compaction/retirement to complete before retrying GC",
                walk.pinned_seq,
            ),
        ));
    }
    let index = DerivedSourceReferenceIndex {
        baseline,
        delta: BTreeMap::new(),
    };
    let census = derived_source_census_from_index(
        &index,
        DerivedSourceCensusProvenance {
            mode,
            pinned_seq: walk.pinned_seq,
            previous_pinned_seq,
            pages: calyx_len_to_u64(CALYX_GC_CF, "Calyx GC census pages", walk.pages)?,
            base_rows_visited: calyx_len_to_u64(
                CALYX_GC_CF,
                "Calyx GC census Base rows",
                walk.rows_visited,
            )?,
            changed_base_keys,
            rebase_reason,
        },
    )?;
    emit_derived_source_census(&index, census)?;
    Ok((
        CalyxGcSourceCensusCache {
            pinned_seq: walk.pinned_seq,
            referenced: index,
        },
        census,
    ))
}

fn derived_source_census_from_index(
    referenced: &DerivedSourceReferenceIndex,
    provenance: DerivedSourceCensusProvenance,
) -> StorageResult<gc::DerivedSourceCensus> {
    Ok(gc::DerivedSourceCensus {
        mode: provenance.mode,
        pinned_seq: provenance.pinned_seq,
        previous_pinned_seq: provenance.previous_pinned_seq,
        pages: provenance.pages,
        base_rows_visited: provenance.base_rows_visited,
        changed_base_keys: provenance.changed_base_keys,
        rebase_reason: provenance.rebase_reason,
        referenced_column_families: calyx_len_to_u64(
            CALYX_GC_CF,
            "Calyx GC census source column families",
            referenced.referenced_column_families(),
        )?,
        referenced_rows: referenced.referenced_rows()?,
    })
}

fn emit_derived_source_census(
    referenced: &DerivedSourceReferenceIndex,
    census: gc::DerivedSourceCensus,
) -> StorageResult<()> {
    let packed = referenced.baseline.metrics;
    let delta_reference_rows = calyx_len_to_u64(
        CALYX_GC_CF,
        "Calyx GC delta protected source rows",
        referenced.delta_reference_count(),
    )?;
    let delta_key_bytes = referenced
        .delta
        .values()
        .flat_map(|keys| keys.iter())
        .try_fold(0_u64, |total, key| {
            let key_bytes = calyx_len_to_u64(
                CALYX_GC_CF,
                "Calyx GC delta protected source key bytes",
                key.len(),
            )?;
            total.checked_add(key_bytes).ok_or_else(|| {
                calyx_write_failed_detail(
                    CALYX_GC_CF,
                    "Calyx GC delta protected source key bytes overflowed u64",
                )
            })
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_GC_SOURCE_CENSUS_COMPLETED",
        mode = census.mode,
        pinned_seq = census.pinned_seq,
        previous_pinned_seq = census.previous_pinned_seq,
        pages = census.pages,
        base_rows_visited = census.base_rows_visited,
        changed_base_keys = census.changed_base_keys,
        rebase_reason = census.rebase_reason,
        referenced_column_families = census.referenced_column_families,
        referenced_rows = census.referenced_rows,
        packed_key_bytes = packed.key_bytes,
        packed_key_capacity_bytes = packed.key_capacity_bytes,
        packed_key_chunks = packed.chunks,
        range_index_bytes = packed.range_bytes,
        range_index_capacity_bytes = packed.range_capacity_bytes,
        mapped_artifact_path = %referenced.baseline.artifact.path().display(),
        mapped_artifact_bytes = referenced.baseline.artifact.file_len(),
        mapped_artifact_pinned_seq = referenced.baseline.pinned_seq,
        mapped_artifact_base_last_commit_seq = referenced.baseline.base_last_commit_seq,
        delta_reference_rows,
        delta_key_bytes,
        delta_reference_bound = CALYX_GC_SOURCE_CENSUS_MAX_DELTA_REFERENCES,
        chunk_bytes = PACKED_SOURCE_REFERENCE_CHUNK_BYTES,
        "refreshed exact derived-source reachability from one pinned committed sequence"
    );
    Ok(())
}

/// Indexes one `Base` row's source reference, if it names one.
fn collect_derived_source_reference(
    value: &[u8],
    referenced: &mut DerivedSourceReferences,
) -> Result<(), synapse_calyx::SynapseCalyxError> {
    let Some((source_cf, source_key)) = decode_derived_source_reference(value)? else {
        return Ok(());
    };
    referenced
        .entry(source_cf.clone())
        .or_default()
        .push(&source_cf, &source_key)
        .map_err(|detail| source_reference_decode_error(&detail))?;
    Ok(())
}

fn source_reference_decode_error(detail: &str) -> synapse_calyx::SynapseCalyxError {
    synapse_calyx::SynapseCalyxError::new(
        "SYNAPSE_CALYX_GC_SOURCE_REFERENCE_UNDECODABLE",
        detail,
        "repair or remove the derived constellation naming an undecodable source key; GC must \
         not treat a corrupt reference as an absent one",
    )
}

fn decode_derived_source_reference(
    value: &[u8],
) -> Result<Option<(String, Vec<u8>)>, synapse_calyx::SynapseCalyxError> {
    let to_error = |detail: String| source_reference_decode_error(&detail);
    {
        let constellation =
            calyx_aster::vault::encode::decode_constellation_base(value).map_err(|source| {
                to_error(format!(
                    "decode Base row while indexing derived source references: {source}"
                ))
            })?;
        let (Some(source_cf), Some(source_key_hex)) = (
            constellation
                .metadata
                .get(crate::constellations::META_SOURCE_CF),
            constellation
                .metadata
                .get(crate::constellations::META_SOURCE_KEY_HEX),
        ) else {
            return Ok(None);
        };
        // A derived row that records an undecodable source key is a corrupt
        // reference, not an absent one. Fail closed rather than let GC treat it
        // as "nothing points here" and delete the source.
        let source_key = decode_source_key_hex(source_key_hex).map_err(|detail| {
            to_error(format!(
                "derived constellation {} records an undecodable {} for source CF {source_cf}: {detail}",
                constellation.cx_id,
                crate::constellations::META_SOURCE_KEY_HEX
            ))
        })?;
        Ok(Some((source_cf.clone(), source_key)))
    }
}

/// Decodes a `synapse_source_key_hex` metadata value back to the raw source
/// key bytes.
///
/// Written here rather than pulled from a hex crate so a malformed value is
/// reported as the specific corruption it is instead of an opaque parse error.
fn decode_source_key_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err(format!(
            "hex length {} is odd, so it cannot encode whole bytes",
            value.len()
        ));
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|error| format!("byte at hex offset {index} is not hex: {error}"))
        })
        .collect()
}

fn run_calyx_gc_budgets(
    vault: &SynapseCalyxVault,
    budgets: &[CalyxGcBudget],
    source_census: &Mutex<Option<CalyxGcSourceCensusCache>>,
) -> StorageResult<gc::GcReport> {
    // The inner scope owns every per-pass GC transient: per-CF retention
    // vectors, eviction proposals, and pending tombstones. The exact packed
    // source baseline is intentionally live in the runner across passes;
    // allocator collection must therefore release only dead ownership.
    let gc_result = run_calyx_gc_budgets_owned(vault, budgets, source_census);
    let release_result = synapse_calyx::release_process_memory("storage_gc_complete");
    match (gc_result, release_result) {
        (Ok(report), Ok(release)) => {
            tracing::info!(
                code = "STORAGE_CALYX_GC_TRANSIENT_MEMORY_RELEASED",
                private_bytes_before = release.private_bytes_before,
                private_bytes_after = release.private_bytes_after,
                private_bytes_reclaimed = release.private_bytes_reclaimed,
                release_elapsed_us = release.elapsed_us,
                "released all dead corpus-sized GC transients after their owner scope ended"
            );
            Ok(report)
        }
        (Err(gc_error), Ok(_release)) => Err(gc_error),
        (Ok(_report), Err(release_error)) => Err(calyx_write_failed(
            CALYX_GC_CF,
            "release dead transient memory after a completed Calyx GC pass",
            &release_error,
        )),
        (Err(gc_error), Err(release_error)) => Err(calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_AND_MEMORY_RELEASE_FAILED: GC failed with {gc_error}; after every GC transient owner was destroyed, allocator release also failed with {release_error}; remediation=repair both independently reported failures before retrying"
            ),
        )),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "all per-CF budgets share one reference census and one atomic tombstone publication decision"
)]
fn run_calyx_gc_budgets_owned(
    vault: &SynapseCalyxVault,
    budgets: &[CalyxGcBudget],
    source_census_cache: &Mutex<Option<CalyxGcSourceCensusCache>>,
) -> StorageResult<gc::GcReport> {
    let now_ms = calyx_clock_now_for_write(vault, CALYX_GC_CF)?;
    let mut source_census_cache = source_census_cache.lock().map_err(|poisoned| {
        calyx_write_failed_detail(
            CALYX_GC_CF,
            format!(
                "STORAGE_CALYX_GC_SOURCE_CENSUS_CACHE_POISONED: exact reachability cache lock was poisoned by an earlier panic: {poisoned}; refusing GC because deletion authority cannot be proven; remediation=inspect the earlier panic and restart the daemon after repair"
            ),
        )
    })?;
    let source_census = if budgets.iter().any(|budget| !budget.protected) {
        Some(refresh_derived_source_references(
            vault,
            &mut source_census_cache,
        )?)
    } else {
        None
    };
    let mut cf_reports = Vec::with_capacity(budgets.len());
    let mut tombstones = Vec::new();
    for budget in budgets {
        if budget.protected {
            tracing::warn!(
                code = "STORAGE_CALYX_GC_PROTECTED_CF_POLICY_SKIPPED",
                cf = budget.cf_name,
                unit = budget.unit.as_str(),
                soft_cap = budget.soft_cap,
                hard_cap = budget.hard_cap,
                reason = CALYX_GC_PROTECTED_CF_POLICY_SKIPPED,
                "generic Calyx GC skipped a policy-protected family before source census lookup or row scan"
            );
            cf_reports.push(gc::GcCfReport {
                cf_name: budget.cf_name.to_owned(),
                before_value: None,
                after_value: None,
                before_estimated_num_keys: None,
                after_estimated_num_keys: None,
                examined_rows: 0,
                scan_limited: false,
                evicted_rows: 0,
                eviction_skipped_reason: Some(CALYX_GC_PROTECTED_CF_POLICY_SKIPPED),
                hard_cap_reached: false,
                hard_cap_code: None,
            });
            continue;
        }
        let referenced = source_census_cache.as_ref().ok_or_else(|| {
            calyx_write_failed_detail(
                CALYX_GC_CF,
                "STORAGE_CALYX_GC_SOURCE_CENSUS_ABSENT: an actionable retention budget reached deletion adjudication without an exact source-reachability census; remediation=repair the GC census state machine before retrying",
            )
        })?;
        cf_reports.push(run_calyx_gc_budget(
            vault,
            *budget,
            now_ms,
            &referenced.referenced,
            &mut tombstones,
        )?);
    }
    // Retention adjudication is the only phase that reads the exact cache.
    // Release its mutex before tombstone I/O and native maintenance; the packed
    // baseline itself intentionally remains owned by the long-lived runner.
    drop(source_census_cache);

    if !tombstones.is_empty() {
        let tombstone_rows =
            calyx_len_to_u64(CALYX_GC_CF, "Calyx GC tombstones", tombstones.len())?;
        commit_calyx_rows_to_vault(vault, CALYX_GC_CF, tombstones)?;
        // Accumulate the newly committed logical deletes and pace the expensive
        // full-CF physical purge (issue #1798): only rewrite the whole KV CF once
        // the deferred backlog crosses a row floor or the max-defer interval
        // elapses, instead of on every tick that evicted any row.
        let pending = CALYX_GC_PENDING_TOMBSTONE_ROWS
            .fetch_add(tombstone_rows, Ordering::AcqRel)
            .saturating_add(tombstone_rows);
        let last_purge_ms = CALYX_GC_LAST_TOMBSTONE_PURGE_MS.load(Ordering::Acquire);
        let since_last_purge_ms = now_ms.saturating_sub(last_purge_ms);
        let force_after_defer =
            last_purge_ms != 0 && since_last_purge_ms >= CALYX_GC_TOMBSTONE_PURGE_MAX_DEFER_MS;
        let should_purge = pending >= CALYX_GC_TOMBSTONE_PURGE_ROW_THRESHOLD
            || last_purge_ms == 0
            || force_after_defer;
        if should_purge {
            vault.purge_kv_tombstones().map_err(|source| {
                calyx_write_failed(CALYX_GC_CF, "purge Calyx KV tombstones after GC", &source)
            })?;
            CALYX_GC_PENDING_TOMBSTONE_ROWS.store(0, Ordering::Release);
            CALYX_GC_LAST_TOMBSTONE_PURGE_MS.store(now_ms, Ordering::Release);
            tracing::info!(
                code = "STORAGE_CALYX_GC_TOMBSTONES_PURGED",
                tombstone_rows,
                purged_backlog_rows = pending,
                since_last_purge_ms,
                forced_after_defer = force_after_defer,
                "Calyx storage GC purged committed KV tombstones"
            );
        } else {
            tracing::info!(
                code = "STORAGE_CALYX_GC_TOMBSTONE_PURGE_DEFERRED",
                tombstone_rows,
                pending_tombstone_rows = pending,
                purge_row_threshold = CALYX_GC_TOMBSTONE_PURGE_ROW_THRESHOLD,
                since_last_purge_ms,
                max_defer_ms = CALYX_GC_TOMBSTONE_PURGE_MAX_DEFER_MS,
                "deferring full-CF Calyx KV tombstone purge under pacing hysteresis; logical deletes already committed"
            );
        }
    }

    // Every per-CF retention vector and committed tombstone payload is now
    // dead. Return those allocator pages before compaction; the exact packed
    // source baseline remains live by design. The executable allocator hook is
    // mandatory and this operation fails closed if the OS readback or release
    // itself fails.
    let retention_release = synapse_calyx::release_process_memory("storage_gc_retention_complete")
        .map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "release dead retention-census memory before native fan-out compaction",
                &source,
            )
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_GC_RETENTION_MEMORY_RELEASED",
        private_bytes_before = retention_release.private_bytes_before,
        private_bytes_after = retention_release.private_bytes_after,
        private_bytes_reclaimed = retention_release.private_bytes_reclaimed,
        release_elapsed_us = retention_release.elapsed_us,
        "released the completed retention phase before native fan-out compaction acquired its working set"
    );

    let native_fanout = vault.compact_native_fanout_once().map_err(|source| {
        calyx_write_failed(
            CALYX_GC_CF,
            "run bounded native Calyx CF fan-out maintenance",
            &source,
        )
    })?;
    tracing::info!(
        code = "STORAGE_CALYX_NATIVE_FANOUT_MAINTENANCE_COMPLETED",
        attempted_cfs = native_fanout.attempted_cfs,
        compacted_cfs = native_fanout.compacted_cfs,
        skipped_cfs = native_fanout.skipped_cfs,
        reclaimed_input_files = native_fanout.reclaimed_input_files,
        input_bytes = native_fanout.input_bytes,
        output_bytes = native_fanout.output_bytes,
        compacted_cf_names = ?native_fanout.compacted_cf_names,
        "Calyx storage GC completed bounded native-CF file-count maintenance"
    );

    let wal_recycle = vault
        .recycle_durable_wal_once(
            CALYX_GC_WAL_RECYCLE_MAX_SEGMENTS,
            CALYX_GC_WAL_RECYCLE_FSYNC_BUDGET,
        )
        .map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "flush checkpoints and recycle durable Calyx WAL segments",
                &source,
            )
        })?;
    tracing::info!(
        code = "STORAGE_CALYX_WAL_RECYCLE_COMPLETED",
        newest_durable_seq = wal_recycle.newest_durable_seq,
        bytes_before = wal_recycle.bytes_before,
        bytes_after = wal_recycle.bytes_after,
        segments_before = wal_recycle.segments_before,
        recyclable_segments_before = wal_recycle.recyclable_segments_before,
        segments_recycled = wal_recycle.segments_recycled,
        bytes_recycled = wal_recycle.bytes_recycled,
        recycled_paths = ?wal_recycle.recycled_paths,
        max_segments = CALYX_GC_WAL_RECYCLE_MAX_SEGMENTS,
        fsync_budget = CALYX_GC_WAL_RECYCLE_FSYNC_BUDGET,
        "Calyx storage GC completed bounded durable WAL recycling"
    );

    Ok(gc::GcReport {
        cf_reports,
        source_census,
        // The eviction pass takes no reclamation decision. `CalyxGcRunner`
        // attaches the pass it runs *after* this one returns (#2122).
        snapshot_version_gc: None,
    })
}

fn run_calyx_gc_budget(
    vault: &SynapseCalyxVault,
    budget: CalyxGcBudget,
    now_ms: u64,
    referenced: &DerivedSourceReferenceIndex,
    pending_tombstones: &mut Vec<SynapseCalyxCfWrite>,
) -> StorageResult<gc::GcCfReport> {
    let collection_id = calyx_collection_id_for_cf_write(budget.cf_name)?;
    let mut state = collect_calyx_retention_state(
        vault,
        budget.cf_name,
        collection_id,
        now_ms,
        budget.protected,
        referenced,
    )?;
    if state.retained_referenced_rows > 0 {
        tracing::info!(
            code = CALYX_GC_SOURCE_ROW_RETAINED_FOR_DERIVED,
            cf = budget.cf_name,
            retained_referenced_rows = state.retained_referenced_rows,
            "Calyx storage GC retained source rows that live derived constellations still point at"
        );
    }
    let materialized_eviction_candidate_rows = calyx_len_to_u64(
        budget.cf_name,
        "Calyx GC materialized eviction candidate row count",
        state.live_entries.len(),
    )?;
    let before_live_rows = state.before_live_rows;
    let expected_materialized_eviction_candidate_rows = if budget.protected {
        0
    } else {
        before_live_rows
    };
    if materialized_eviction_candidate_rows != expected_materialized_eviction_candidate_rows {
        return Err(calyx_write_failed_detail(
            budget.cf_name,
            format!(
                "Calyx retention materialization invariant violated: protected={} live_rows={before_live_rows} materialized_eviction_candidate_rows={materialized_eviction_candidate_rows} expected={expected_materialized_eviction_candidate_rows}",
                budget.protected
            ),
        ));
    }
    let before_estimated_num_keys = before_live_rows
        .checked_add(state.expired_rows)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                budget.cf_name,
                format!(
                    "Calyx GC key-count accounting overflow: live_rows={before_live_rows} expired_rows={}",
                    state.expired_rows
                ),
            )
        })?;
    let before_value = match budget.unit {
        CalyxGcUnit::Bytes => state.before_live_bytes,
        CalyxGcUnit::Rows => before_live_rows,
    };
    let hard_cap_reached = log_calyx_gc_hard_cap_if_reached(budget, before_value);
    let cap_outcome = apply_calyx_gc_cap_eviction(vault, now_ms, budget, &mut state, before_value)?;
    let evicted_rows = state
        .expired_rows
        .checked_add(cap_outcome.cap_evicted_rows)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                budget.cf_name,
                format!(
                    "Calyx GC evicted-row accounting overflow: expired_rows={} cap_evicted_rows={}",
                    state.expired_rows, cap_outcome.cap_evicted_rows
                ),
            )
        })?;
    let after_estimated_num_keys = before_estimated_num_keys.saturating_sub(evicted_rows);
    emit_calyx_gc_report(
        budget,
        &state,
        &cap_outcome,
        before_live_rows,
        materialized_eviction_candidate_rows,
        before_value,
        hard_cap_reached,
    );
    if cap_outcome.cap_evicted_rows > 0 {
        emit_calyx_gc_eviction_metric(
            budget,
            cap_outcome.cap_evicted_rows,
            before_value,
            cap_outcome.after_value,
        );
    }
    pending_tombstones.append(&mut state.tombstones);

    Ok(gc::GcCfReport {
        cf_name: budget.cf_name.to_owned(),
        before_value: Some(before_value),
        after_value: Some(cap_outcome.after_value),
        before_estimated_num_keys: Some(before_estimated_num_keys),
        after_estimated_num_keys: Some(after_estimated_num_keys),
        examined_rows: before_estimated_num_keys,
        scan_limited: false,
        evicted_rows,
        eviction_skipped_reason: cap_outcome.eviction_skipped_reason,
        hard_cap_reached,
        hard_cap_code: hard_cap_reached.then_some(error_codes::STORAGE_CF_HARD_CAP_REACHED),
    })
}

/// Drains the oldest-written live rows until the family is back under its soft
/// cap, re-checking each row against its current bytes before proposing its
/// tombstone (#2060).
///
/// The drain order and the caps are unchanged. What changed is that
/// `live_entries` now comes from a paged sweep rather than one wide-guard scan,
/// so an entry may describe bytes that have since been replaced. A superseded
/// entry is **skipped without being charged against the cap** — it is not
/// evicted, so its bytes were never freed, so subtracting them would leave the
/// loop believing it had reached the soft cap when it had not. Skipping keeps
/// draining down the age order until the cap is genuinely met, which is exactly
/// the behaviour the unpaged shape had.
fn apply_calyx_gc_cap_eviction(
    vault: &SynapseCalyxVault,
    now_ms: u64,
    budget: CalyxGcBudget,
    state: &mut CalyxRetentionState,
    before_value: u64,
) -> StorageResult<CalyxGcCapOutcome> {
    let mut outcome = CalyxGcCapOutcome {
        after_value: before_value,
        cap_evicted_rows: 0,
        eviction_skipped_reason: None,
    };
    if before_value <= budget.soft_cap {
        return Ok(outcome);
    }
    if budget.protected {
        tracing::warn!(
            code = "STORAGE_CALYX_GC_PROTECTED_CAP_SKIPPED",
            cf = budget.cf_name,
            unit = budget.unit.as_str(),
            before_value,
            soft_cap = budget.soft_cap,
            hard_cap = budget.hard_cap,
            reason = CALYX_GC_PROTECTED_CF_POLICY_SKIPPED,
            "Calyx storage GC skipped cap eviction for a policy-protected column family"
        );
        outcome.eviction_skipped_reason = Some(CALYX_GC_PROTECTED_CF_POLICY_SKIPPED);
        return Ok(outcome);
    }

    state.live_entries.sort_by(|left, right| {
        left.written_at_ms
            .cmp(&right.written_at_ms)
            .then_with(|| left.user_key.cmp(&right.user_key))
    });
    let entries = std::mem::take(&mut state.live_entries);
    for entry in entries {
        if outcome.after_value <= budget.soft_cap {
            break;
        }
        let candidate = CalyxGcEvictionCandidate {
            full_key: entry.full_key,
            value_digest: entry.value_digest,
        };
        let Some(tombstone) = confirm_calyx_gc_eviction(
            vault,
            budget.cf_name,
            now_ms,
            &candidate,
            &mut state.revalidation,
        )?
        else {
            continue;
        };
        let removed_value = match budget.unit {
            CalyxGcUnit::Bytes => entry.live_bytes,
            CalyxGcUnit::Rows => 1,
        };
        outcome.after_value = outcome.after_value.checked_sub(removed_value).ok_or_else(|| {
            calyx_write_failed_detail(
                budget.cf_name,
                format!(
                    "Calyx GC {} accounting underflow while evicting value {removed_value} from {}",
                    budget.unit.as_str(),
                    budget.cf_name
                ),
            )
        })?;
        state.tombstones.push(tombstone);
        outcome.cap_evicted_rows = outcome.cap_evicted_rows.saturating_add(1);
    }

    if before_value > budget.hard_cap && outcome.after_value > budget.hard_cap {
        let detail = format!(
            "Calyx GC could not reduce {cf} below hard cap: unit={unit} before_value={before_value} after_value={after_value} hard_cap={hard_cap}",
            cf = budget.cf_name,
            unit = budget.unit.as_str(),
            after_value = outcome.after_value,
            hard_cap = budget.hard_cap
        );
        tracing::error!(
            code = error_codes::STORAGE_WRITE_FAILED,
            cf = budget.cf_name,
            unit = budget.unit.as_str(),
            before_value,
            after_value = outcome.after_value,
            hard_cap = budget.hard_cap,
            detail,
            "Calyx storage GC hard cap enforcement failed"
        );
        return Err(calyx_write_failed_detail(budget.cf_name, detail));
    }
    Ok(outcome)
}

fn log_calyx_gc_hard_cap_if_reached(budget: CalyxGcBudget, before_value: u64) -> bool {
    let hard_cap_reached = before_value >= budget.hard_cap;
    if hard_cap_reached {
        tracing::warn!(
            code = error_codes::STORAGE_CF_HARD_CAP_REACHED,
            cf = budget.cf_name,
            unit = budget.unit.as_str(),
            before_value,
            soft_cap = budget.soft_cap,
            hard_cap = budget.hard_cap,
            protected = budget.protected,
            "Calyx storage GC hard cap reached"
        );
    }
    hard_cap_reached
}

fn emit_calyx_gc_report(
    budget: CalyxGcBudget,
    state: &CalyxRetentionState,
    cap_outcome: &CalyxGcCapOutcome,
    before_live_rows: u64,
    materialized_eviction_candidate_rows: u64,
    before_value: u64,
    hard_cap_reached: bool,
) {
    if state.expired_rows > 0
        || cap_outcome.cap_evicted_rows > 0
        || cap_outcome.eviction_skipped_reason.is_some()
        || hard_cap_reached
        || state.revalidation.superseded > 0
        || state.revalidation.vanished > 0
    {
        tracing::info!(
            code = "STORAGE_CALYX_GC_COMPLETED",
            cf = budget.cf_name,
            unit = budget.unit.as_str(),
            expired_rows = state.expired_rows,
            cap_evicted_rows = cap_outcome.cap_evicted_rows,
            before_live_rows,
            materialized_eviction_candidate_rows,
            before_value,
            after_value = cap_outcome.after_value,
            soft_cap = budget.soft_cap,
            hard_cap = budget.hard_cap,
            hard_cap_reached,
            eviction_skipped_reason = cap_outcome.eviction_skipped_reason.unwrap_or("none"),
            // Provenance of the pinned stream the decisions were folded from,
            // and what the pre-delete re-check changed about them (#2060).
            // `sweep_atomic` is reported as an independently inspectable
            // invariant rather than assumed.
            sweep_pages = state.sweep.pages,
            sweep_rows_visited = state.sweep.rows_visited,
            sweep_snapshot_seq_first = state.sweep.snapshot_seq_first,
            sweep_snapshot_seq_last = state.sweep.snapshot_seq_last,
            sweep_atomic = state.sweep.atomic(),
            revalidated_rows = state.revalidation.checked,
            revalidation_superseded_rows = state.revalidation.superseded,
            revalidation_vanished_rows = state.revalidation.vanished,
            "Calyx storage GC report completed"
        );
    }
}

fn emit_calyx_gc_eviction_metric(
    budget: CalyxGcBudget,
    cap_evicted_rows: u64,
    before_value: u64,
    after_value: u64,
) {
    synapse_telemetry::metrics::counter!(
        CALYX_GC_CACHE_EVICTIONS_TOTAL,
        "cf" => budget.cf_name,
        "reason" => CALYX_GC_SOFT_CAP_REASON
    )
    .increment(cap_evicted_rows);
    tracing::info!(
        code = "STORAGE_CACHE_EVICTIONS_TOTAL_INCREMENTED",
        metric_name = CALYX_GC_CACHE_EVICTIONS_TOTAL,
        cf = budget.cf_name,
        reason = CALYX_GC_SOFT_CAP_REASON,
        delta = cap_evicted_rows,
        before_value,
        after_value,
        "Calyx storage GC cache eviction counter incremented"
    );
}

/// Folds one column family's ordered Calyx KV namespace into its retention
/// state **page by page**, releasing the vault-wide row-table read guard
/// between pages (#2060).
///
/// This was the GC eviction phase's `scan_cf_range_latest`: one acquisition of
/// the shared MVCC row-table read guard held across the merge and
/// materialisation of an entire namespace. On the live vault that measured
/// 189/191/297 ms holds against a 25 ms budget and took the site's worst-ever
/// hold to 303 ms — and because every commit must take that same guard
/// exclusively, each of those holds stalled every writer in the daemon for as
/// long as it ran. #2041 had already proved the remedy on the same corpus:
/// [`sweep_kv_range_pages`] answers the identical question at ~137 us mean per
/// hold with zero over-budget holds.
///
/// **How eviction stays correct.** The complete fold now observes one pinned
/// committed sequence, but a row can still be rewritten after the stream reads
/// it and before the later tombstone commit. The proposal is therefore
/// separated from the delete: every candidate is re-checked against the row's
/// *current* bytes under a short point-read guard immediately before its
/// tombstone is written
/// ([`confirm_calyx_gc_eviction`]), and a row whose bytes changed is retained
/// for the next tick rather than deleted on stale evidence. Retention is the
/// safe direction — a row kept one interval too long is recoverable, a row
/// deleted on a stale view is not.
///
/// The sweep's pinned sequence and atomicity proof are returned in
/// [`CalyxRetentionState::sweep`] rather than inferred from a successful call.
fn collect_calyx_retention_state(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    collection_id: u64,
    now_ms: u64,
    protected: bool,
    referenced: &DerivedSourceReferenceIndex,
) -> StorageResult<CalyxRetentionState> {
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let mut state = CalyxRetentionState {
        before_live_rows: 0,
        live_entries: Vec::new(),
        expired_candidates: Vec::new(),
        tombstones: Vec::new(),
        before_live_bytes: 0,
        expired_rows: 0,
        retained_referenced_rows: 0,
        sweep: CalyxKvSweep::default(),
        revalidation: CalyxGcEvictionRevalidation::default(),
    };
    let sweep = sweep_kv_range_pages(
        vault,
        cf_name,
        CALYX_GC_RETENTION_SWEEP_SITE,
        &range,
        |full_key, value| {
            let user_key = decode_calyx_user_key(collection_id, full_key).map_err(|detail| {
                calyx_write_failed_detail(
                    cf_name,
                    format!("decode Calyx retention scan key: {detail}"),
                )
            })?;
            let envelope = decode_calyx_value_raw(value).map_err(|detail| {
                tracing::error!(
                    code = error_codes::STORAGE_WRITE_FAILED,
                    cf = cf_name,
                    detail,
                    "Calyx storage backend rejected malformed KV retention envelope during enforcement"
                );
                calyx_write_failed_detail(
                    cf_name,
                    format!("decode Calyx retention envelope: {detail}"),
                )
            })?;
            // #1882: a source row a live derived constellation still points at
            // is not garbage, however old it is. Deleting it destroys the only
            // copy of the input bytes that row's CxId addresses, so re-derive,
            // lazy backfill and derivation audit all become impossible with no
            // error at the moment of loss. Retained rows are excluded from cap
            // eviction too, and counted so the retention is reported rather
            // than silent.
            let referenced_by_derived = referenced.contains(cf_name, &user_key);
            if referenced_by_derived {
                state.retained_referenced_rows = state.retained_referenced_rows.saturating_add(1);
                return Ok(ControlFlow::Continue(()));
            }
            if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
                if !protected {
                    state.expired_candidates.push(CalyxGcEvictionCandidate {
                        full_key: full_key.to_vec(),
                        value_digest: calyx_gc_row_digest(value),
                    });
                }
                return Ok(ControlFlow::Continue(()));
            }
            let live_bytes = calyx_live_row_bytes(cf_name, &user_key, envelope.payload)?;
            state.before_live_bytes =
                state
                    .before_live_bytes
                    .checked_add(live_bytes)
                    .ok_or_else(|| {
                        calyx_write_failed_detail(
                            cf_name,
                            format!("Calyx retention live-byte accounting overflow in {cf_name}"),
                        )
                    })?;
            state.before_live_rows = state.before_live_rows.checked_add(1).ok_or_else(|| {
                calyx_write_failed_detail(
                    cf_name,
                    format!("Calyx retention live-row accounting overflow in {cf_name}"),
                )
            })?;
            // A protected family cannot enter cap eviction, so retaining its
            // full key, user key, digest and timestamps until the sweep ends is
            // dead ownership. Keep the exact scalar count/bytes above and only
            // allocate row candidates for the branch that can consume them.
            if !protected {
                state.live_entries.push(CalyxRetentionLiveEntry {
                    full_key: full_key.to_vec(),
                    user_key,
                    live_bytes,
                    written_at_ms: envelope.written_at_ms,
                    value_digest: calyx_gc_row_digest(value),
                });
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    state.sweep = sweep;
    confirm_calyx_gc_expired_evictions(vault, cf_name, now_ms, &mut state)?;
    Ok(state)
}

/// `site` label for the GC eviction phase's paged namespace fold.
///
/// This names the *sweep*, in `STORAGE_CALYX_BOUNDED_SWEEP` and in any
/// cursor-failure error, so a fold reported there is attributable to the GC
/// tick rather than to a diagnostic an operator ran — the two have completely
/// different remedies, which is the same reason #1973 split
/// `scan_cf_at_overlay` out of the shared overlay site.
///
/// It is deliberately **not** a new `calyx_row_guard_sites` entry. The guard
/// this fold now takes is `scan_cf_range_page_latest`'s, and the whole claim of
/// this change is that GC's holds have moved onto that already-measured,
/// already-in-budget site. Minting a private census name would hide exactly the
/// number the fix has to be judged on: `scan_cf_range_latest` losing GC's holds
/// while `scan_cf_range_page_latest` gains them, with `over_budget_holds` still
/// zero.
const CALYX_GC_RETENTION_SWEEP_SITE: &str = "calyx_gc_retention_namespace";

/// Digest of the exact stored bytes an eviction decision was taken from
/// (#2060).
///
/// FNV-1a over the whole retention envelope — version byte, expiry, write
/// timestamp and payload — rather than a field comparison, because the v1
/// envelope carries no `written_at_ms` at all (it decodes as 0), so comparing
/// timestamps would silently fail to notice a rewrite of any v1 row. Written
/// here rather than pulled from a hash crate because this value never leaves
/// the process, is never persisted, and is compared only against another
/// digest taken by this same function moments earlier.
fn calyx_gc_row_digest(value: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut digest = FNV_OFFSET_BASIS;
    for byte in value {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(FNV_PRIME);
    }
    digest
}

/// Re-checks one eviction proposal against the row's **current** bytes under a
/// short point-read guard, and returns the tombstone only if they still match
/// (#2060).
///
/// This is what makes a paged retention sweep safe to delete from. The proposal
/// was formed from bytes read on some page, at some committed sequence; between
/// then and now any writer may have replaced the row. Writing the tombstone
/// anyway would destroy a value GC never examined — a value that may not be
/// expired, may be referenced, and is certainly not the one the eviction rule
/// was evaluated against.
///
/// A point read takes the row guard for a single key lookup, which is the
/// `read_latest` site — already measured well inside budget — so the re-check
/// preserves the bounded-hold property the paging exists to establish.
///
/// Fails closed on an undecodable current envelope: a row whose stored bytes no
/// longer decode is a corruption finding, not a licence to delete it.
fn confirm_calyx_gc_eviction(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    now_ms: u64,
    candidate: &CalyxGcEvictionCandidate,
    revalidation: &mut CalyxGcEvictionRevalidation,
) -> StorageResult<Option<SynapseCalyxCfWrite>> {
    revalidation.checked = revalidation.checked.saturating_add(1);
    let current = vault
        .read_cf_latest(ColumnFamily::Kv, &candidate.full_key)
        .map_err(|source| {
            calyx_write_failed(
                cf_name,
                "re-read a Calyx GC eviction candidate before writing its tombstone",
                &source,
            )
        })?;
    let Some(current) = current else {
        revalidation.vanished = revalidation.vanished.saturating_add(1);
        tracing::debug!(
            code = "STORAGE_CALYX_GC_EVICTION_CANDIDATE_VANISHED",
            cf = cf_name,
            key_hex = hex_prefix_for_log(&candidate.full_key),
            "a Calyx GC eviction candidate was already gone at the pre-delete re-check; no \
             tombstone written"
        );
        return Ok(None);
    };
    // Decoded, not merely digested: an undecodable current row must be reported
    // as the corruption it is rather than counted as a benign supersede.
    decode_calyx_value_raw(&current).map_err(|detail| {
        tracing::error!(
            code = error_codes::STORAGE_WRITE_FAILED,
            cf = cf_name,
            detail,
            "Calyx storage backend rejected a malformed KV retention envelope while re-checking a \
             GC eviction candidate"
        );
        calyx_write_failed_detail(
            cf_name,
            format!("decode Calyx retention envelope at the GC pre-delete re-check: {detail}"),
        )
    })?;
    if calyx_gc_row_digest(&current) != candidate.value_digest {
        revalidation.superseded = revalidation.superseded.saturating_add(1);
        tracing::warn!(
            code = "STORAGE_CALYX_GC_EVICTION_CANDIDATE_SUPERSEDED",
            cf = cf_name,
            key_hex = hex_prefix_for_log(&candidate.full_key),
            now_ms,
            "a Calyx GC eviction candidate was rewritten between the page that proposed it and \
             its pre-delete re-check; the row is retained for the next tick rather than deleted \
             on a stale view"
        );
        return Ok(None);
    }
    Ok(Some(SynapseCalyxCfWrite::new(
        ColumnFamily::Kv,
        candidate.full_key.clone(),
        tombstone_value(),
    )))
}

/// Confirms every expired-row proposal and turns the survivors into tombstones.
///
/// `expired_rows` is incremented only for confirmed deletes, so the GC report
/// counts rows this tick actually tombstoned rather than rows it proposed.
fn confirm_calyx_gc_expired_evictions(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    now_ms: u64,
    state: &mut CalyxRetentionState,
) -> StorageResult<()> {
    let candidates = std::mem::take(&mut state.expired_candidates);
    for candidate in &candidates {
        if let Some(tombstone) =
            confirm_calyx_gc_eviction(vault, cf_name, now_ms, candidate, &mut state.revalidation)?
        {
            state.tombstones.push(tombstone);
            state.expired_rows = state.expired_rows.saturating_add(1);
        }
    }
    Ok(())
}

fn calyx_retention_default_for_write(cf_name: &str) -> StorageResult<RetentionDefault> {
    let retention = DEFAULTS
        .iter()
        .copied()
        .find(|default| default.cf == cf_name)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                cf_name,
                format!("missing RetentionDefault mapping for Calyx column family {cf_name}"),
            )
        })?;
    validate_calyx_retention_cap_entry(retention)?;
    Ok(retention)
}

fn validate_calyx_retention_cap_coupling() -> StorageResult<()> {
    for retention in DEFAULTS {
        let definitions = DEFAULTS
            .iter()
            .filter(|candidate| candidate.cf == retention.cf)
            .count();
        if definitions != 1 {
            return Err(calyx_write_failed_detail(
                retention.cf,
                format!(
                    "CALYX_RETENTION_DEFAULT_DUPLICATE: cf={} definitions={definitions}; remediation=retain exactly one authoritative policy per column family",
                    retention.cf
                ),
            ));
        }
        validate_calyx_retention_cap_entry(retention)?;
    }
    Ok(())
}

fn validate_calyx_retention_cap_entry(retention: RetentionDefault) -> StorageResult<()> {
    let RetentionCapEviction::LockstepWith(peer_cf) = retention.cap_eviction else {
        return Ok(());
    };
    if peer_cf == retention.cf {
        return Err(calyx_write_failed_detail(
            retention.cf,
            format!(
                "CALYX_RETENTION_LOCKSTEP_SELF_REFERENCE: cf={} peer_cf={peer_cf}; remediation=declare the exact source/index peer",
                retention.cf
            ),
        ));
    }
    let peer = DEFAULTS
        .iter()
        .copied()
        .find(|candidate| candidate.cf == peer_cf)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                retention.cf,
                format!(
                    "CALYX_RETENTION_LOCKSTEP_PEER_MISSING: cf={} peer_cf={peer_cf}; remediation=add the peer RetentionDefault and its symmetric LockstepWith declaration",
                    retention.cf
                ),
            )
        })?;
    if peer.cap_eviction != RetentionCapEviction::LockstepWith(retention.cf) {
        return Err(calyx_write_failed_detail(
            retention.cf,
            format!(
                "CALYX_RETENTION_LOCKSTEP_ASYMMETRIC: cf={} peer_cf={peer_cf} peer_policy={:?}; remediation=declare LockstepWith symmetrically on both source and index",
                retention.cf, peer.cap_eviction
            ),
        ));
    }
    if peer.ttl != retention.ttl {
        return Err(calyx_write_failed_detail(
            retention.cf,
            format!(
                "CALYX_RETENTION_LOCKSTEP_TTL_MISMATCH: cf={} ttl={:?} peer_cf={peer_cf} peer_ttl={:?}; remediation=give the source and index one identical logical expiry boundary",
                retention.cf, retention.ttl, peer.ttl
            ),
        ));
    }
    Ok(())
}

fn calyx_retention_cap_bytes_for_write(retention: RetentionDefault) -> StorageResult<(u64, u64)> {
    let soft = retention.soft_cap_mb.checked_mul(MIB_U64).ok_or_else(|| {
        calyx_write_failed_detail(
            retention.cf,
            format!(
                "soft cap overflow for {}: {} MiB",
                retention.cf, retention.soft_cap_mb
            ),
        )
    })?;
    let hard = retention.hard_cap_mb.checked_mul(MIB_U64).ok_or_else(|| {
        calyx_write_failed_detail(
            retention.cf,
            format!(
                "hard cap overflow for {}: {} MiB",
                retention.cf, retention.hard_cap_mb
            ),
        )
    })?;
    if hard < soft {
        return Err(calyx_write_failed_detail(
            retention.cf,
            format!(
                "invalid RetentionDefault for {}: hard cap {} bytes is below soft cap {} bytes",
                retention.cf, hard, soft
            ),
        ));
    }
    Ok((soft, hard))
}

fn calyx_expires_at_ms_for_write(cf_name: &str, now_ms: u64) -> StorageResult<u64> {
    let retention = calyx_retention_default_for_write(cf_name)?;
    let Some(ttl_ms) = calyx_ttl_millis_for_write(cf_name, retention.ttl)? else {
        return Ok(0);
    };
    now_ms.checked_add(ttl_ms).ok_or_else(|| {
        calyx_write_failed_detail(
            cf_name,
            format!("Calyx retention expires_at_ms overflow: now_ms={now_ms} ttl_ms={ttl_ms}"),
        )
    })
}

fn calyx_ttl_millis_for_write(cf_name: &str, ttl: RetentionTtl) -> StorageResult<Option<u64>> {
    match ttl {
        RetentionTtl::None | RetentionTtl::LruOnly => Ok(None),
        RetentionTtl::Hours(hours) => {
            if hours == 0 {
                return Err(calyx_write_failed_detail(
                    cf_name,
                    "RetentionDefault Hours(0) is invalid for Calyx TTL".to_owned(),
                ));
            }
            hours.checked_mul(MILLIS_PER_HOUR).map(Some).ok_or_else(|| {
                calyx_write_failed_detail(
                    cf_name,
                    format!("Calyx retention Hours({hours}) overflows milliseconds"),
                )
            })
        }
        RetentionTtl::Days(days) => {
            if days == 0 {
                return Err(calyx_write_failed_detail(
                    cf_name,
                    "RetentionDefault Days(0) is invalid for Calyx TTL".to_owned(),
                ));
            }
            days.checked_mul(MILLIS_PER_DAY).map(Some).ok_or_else(|| {
                calyx_write_failed_detail(
                    cf_name,
                    format!("Calyx retention Days({days}) overflows milliseconds"),
                )
            })
        }
    }
}

fn calyx_live_row_bytes(cf_name: &str, user_key: &[u8], payload: &[u8]) -> StorageResult<u64> {
    let key_bytes = u64::try_from(user_key.len()).map_err(|_error| {
        calyx_write_failed_detail(
            cf_name,
            format!(
                "Calyx retention key length does not fit in u64: {}",
                user_key.len()
            ),
        )
    })?;
    let payload_bytes = u64::try_from(payload.len()).map_err(|_error| {
        calyx_write_failed_detail(
            cf_name,
            format!(
                "Calyx retention payload length does not fit in u64: {}",
                payload.len()
            ),
        )
    })?;
    key_bytes.checked_add(payload_bytes).ok_or_else(|| {
        calyx_write_failed_detail(
            cf_name,
            format!(
                "Calyx retention row byte accounting overflow: key_bytes={key_bytes} payload_bytes={payload_bytes}"
            ),
        )
    })
}

fn calyx_len_to_u64(cf_name: &str, context: &'static str, len: usize) -> StorageResult<u64> {
    u64::try_from(len).map_err(|_error| {
        calyx_write_failed_detail(cf_name, format!("{context} does not fit in u64: {len}"))
    })
}

fn calyx_cf_protected_from_auto_delete(cf_name: &str) -> bool {
    // CF_KV is the daemon control-plane namespace: workspace rows, mailbox
    // rows, task state, profile/config rows, replay controls, cost prices, and
    // secondary-index rows all live here today. A generic LRU cap cannot know
    // which keys are rebuildable, so automatic Calyx GC must preserve the whole
    // family until each high-volume prefix has an explicit typed store.
    // Every strict source/index pair declared `LockstepWith` is one logical
    // relation. Generic per-CF LRU cannot know the peer key, so cap eviction is
    // forbidden for both sides. Their validated identical row TTL remains the
    // logical expiry authority, and complete compaction reclaims expired bytes.
    matches!(cf_name, cf::CF_KV | cf::CF_ROUTINE_STATE)
        || DEFAULTS.iter().any(|retention| {
            retention.cf == cf_name
                && matches!(
                    retention.cap_eviction,
                    RetentionCapEviction::LockstepWith(_)
                )
        })
}

fn calyx_put_row(
    cf_name: &str,
    collection_id: u64,
    key: &[u8],
    value: &[u8],
    now_ms: u64,
) -> StorageResult<SynapseCalyxCfWrite> {
    let expires_at_ms = calyx_expires_at_ms_for_write(cf_name, now_ms)?;
    Ok(SynapseCalyxCfWrite::new(
        ColumnFamily::Kv,
        encode_calyx_key_for_write(cf_name, collection_id, key)?,
        encode_calyx_value(expires_at_ms, now_ms, value),
    ))
}

fn calyx_put_row_with_expiry(
    cf_name: &str,
    collection_id: u64,
    row: &RawRowWithExpiry,
    now_ms: u64,
) -> StorageResult<SynapseCalyxCfWrite> {
    let default_expires_at_ms = calyx_expires_at_ms_for_write(cf_name, now_ms)?;
    let expires_at_ms = match row.expires_at_ms {
        None => default_expires_at_ms,
        Some(explicit) => {
            if default_expires_at_ms == 0 {
                return Err(calyx_write_failed_detail(
                    cf_name,
                    format!(
                        "CALYX_EXPLICIT_EXPIRY_FOR_UNRETAINED_CF: explicit={explicit}; remediation=use the destination's normal no-TTL write or choose a TTL-bound derived-index CF"
                    ),
                ));
            }
            if explicit <= now_ms {
                return Err(calyx_write_failed_detail(
                    cf_name,
                    format!(
                        "CALYX_EXPLICIT_EXPIRY_ELAPSED: explicit={explicit} now_ms={now_ms}; remediation=omit the already-expired source from the backfill chunk"
                    ),
                ));
            }
            if explicit > default_expires_at_ms {
                return Err(calyx_write_failed_detail(
                    cf_name,
                    format!(
                        "CALYX_EXPLICIT_EXPIRY_EXTENDS_RETENTION: explicit={explicit} maximum={default_expires_at_ms}; remediation=preserve the source expiry exactly and never extend a derived pointer beyond destination policy"
                    ),
                ));
            }
            explicit
        }
    };
    Ok(SynapseCalyxCfWrite::new(
        ColumnFamily::Kv,
        encode_calyx_key_for_write(cf_name, collection_id, &row.key)?,
        encode_calyx_value(expires_at_ms, now_ms, &row.value),
    ))
}

fn calyx_delete_row(
    cf_name: &str,
    collection_id: u64,
    key: &[u8],
) -> StorageResult<SynapseCalyxCfWrite> {
    Ok(SynapseCalyxCfWrite::new(
        ColumnFamily::Kv,
        encode_calyx_key_for_write(cf_name, collection_id, key)?,
        tombstone_value(),
    ))
}

fn encode_calyx_key_for_read(
    cf_name: &str,
    collection_id: u64,
    user_key: &[u8],
) -> StorageResult<Vec<u8>> {
    encode_calyx_key(collection_id, user_key).map_err(|detail| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail,
    })
}

fn encode_calyx_key_for_write(
    cf_name: &str,
    collection_id: u64,
    user_key: &[u8],
) -> StorageResult<Vec<u8>> {
    encode_calyx_key(collection_id, user_key).map_err(|detail| StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
        detail,
    })
}

fn encode_calyx_key(collection_id: u64, user_key: &[u8]) -> Result<Vec<u8>, String> {
    if user_key.len() > CALYX_MAX_USER_KEY_BYTES {
        return Err(format!(
            "Calyx Synapse KV envelope supports keys up to {CALYX_MAX_USER_KEY_BYTES} bytes; got {}",
            user_key.len()
        ));
    }
    let mut key = calyx_namespace_prefix(collection_id);
    key.extend_from_slice(user_key);
    Ok(key)
}

fn calyx_namespace_prefix(collection_id: u64) -> Vec<u8> {
    calyx_namespace_prefix_for(collection_id, CALYX_KV_ORDERED_NAMESPACE)
}

fn calyx_legacy_namespace_prefix(collection_id: u64) -> Vec<u8> {
    calyx_namespace_prefix_for(collection_id, CALYX_KV_LEGACY_LENGTH_ORDERED_NAMESPACE)
}

fn calyx_namespace_prefix_for(collection_id: u64, namespace: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 8 + 8);
    key.push(CALYX_KV_DISC);
    key.extend_from_slice(&collection_id.to_be_bytes());
    key.extend_from_slice(&namespace.to_be_bytes());
    key
}

fn encode_calyx_legacy_key(collection_id: u64, user_key: &[u8]) -> Result<Vec<u8>, String> {
    let user_key_len = u16::try_from(user_key.len()).map_err(|_error| {
        format!(
            "legacy Calyx Synapse KV envelope supports keys up to {} bytes; got {}",
            u16::MAX,
            user_key.len()
        )
    })?;
    let mut key = calyx_legacy_namespace_prefix(collection_id);
    key.extend_from_slice(&user_key_len.to_be_bytes());
    key.extend_from_slice(user_key);
    Ok(key)
}

fn decode_calyx_user_key_for_read(
    cf_name: &str,
    collection_id: u64,
    full_key: &[u8],
) -> StorageResult<Vec<u8>> {
    decode_calyx_user_key(collection_id, full_key).map_err(|detail| StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail,
    })
}

fn decode_calyx_user_key(collection_id: u64, full_key: &[u8]) -> Result<Vec<u8>, String> {
    let prefix = calyx_namespace_prefix(collection_id);
    let Some(rest) = full_key.strip_prefix(prefix.as_slice()) else {
        return Err(
            "Calyx KV scan returned a key outside the requested ordered namespace".to_owned(),
        );
    };
    if rest.len() > CALYX_MAX_USER_KEY_BYTES {
        return Err(format!(
            "ordered Calyx KV key exceeds the supported logical key maximum: len={} maximum={CALYX_MAX_USER_KEY_BYTES}",
            rest.len()
        ));
    }
    Ok(rest.to_vec())
}

fn decode_calyx_legacy_user_key(collection_id: u64, full_key: &[u8]) -> Result<Vec<u8>, String> {
    let prefix = calyx_legacy_namespace_prefix(collection_id);
    let Some(rest) = full_key.strip_prefix(prefix.as_slice()) else {
        return Err("legacy Calyx KV scan returned a key outside namespace zero".to_owned());
    };
    let Some(len_bytes) = rest.get(0..2) else {
        return Err("legacy Calyx KV key is missing its user-key length prefix".to_owned());
    };
    let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    let Some(user_key) = rest.get(2..2 + len) else {
        return Err("legacy Calyx KV key length prefix exceeds the stored key".to_owned());
    };
    if rest.len() != 2 + len {
        return Err("legacy Calyx KV key has trailing bytes after the user key".to_owned());
    }
    Ok(user_key.to_vec())
}

fn encode_calyx_value(expires_at_ms: u64, written_at_ms: u64, payload: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(CALYX_KV_VALUE_HEADER_BYTES + payload.len());
    value.push(CALYX_KV_VALUE_VERSION);
    value.extend_from_slice(&expires_at_ms.to_be_bytes());
    value.extend_from_slice(&written_at_ms.to_be_bytes());
    value.extend_from_slice(payload);
    value
}

fn decode_calyx_value_for_read(
    cf_name: &str,
    value: &[u8],
    now_ms: u64,
) -> StorageResult<Option<Vec<u8>>> {
    let envelope = decode_calyx_value_raw(value).map_err(|detail| {
        tracing::error!(
            code = error_codes::STORAGE_READ_FAILED,
            cf = cf_name,
            detail,
            "Calyx storage backend rejected malformed KV retention envelope"
        );
        StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail,
        }
    })?;
    if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
        return Ok(None);
    }
    Ok(Some(envelope.payload.to_vec()))
}

#[derive(Debug)]
struct CalyxValueEnvelope<'a> {
    expires_at_ms: u64,
    written_at_ms: u64,
    payload: &'a [u8],
}

fn decode_calyx_value_raw(value: &[u8]) -> Result<CalyxValueEnvelope<'_>, String> {
    let Some(version) = value.first().copied() else {
        return Err(
            "Calyx KV value is empty and missing its retention envelope version".to_owned(),
        );
    };
    match version {
        CALYX_KV_VALUE_VERSION_V1 => decode_calyx_value_v1(value),
        CALYX_KV_VALUE_VERSION => decode_calyx_value_v2(value),
        other => Err(format!(
            "Calyx KV value version {other} is unsupported; expected {CALYX_KV_VALUE_VERSION_V1} or {CALYX_KV_VALUE_VERSION}"
        )),
    }
}

fn decode_calyx_value_v1(value: &[u8]) -> Result<CalyxValueEnvelope<'_>, String> {
    if value.len() < CALYX_KV_VALUE_V1_HEADER_BYTES {
        return Err(format!(
            "Calyx KV v1 value is shorter than its {CALYX_KV_VALUE_V1_HEADER_BYTES} byte header"
        ));
    }
    let mut expires_at_bytes = [0_u8; 8];
    expires_at_bytes.copy_from_slice(&value[1..9]);
    Ok(CalyxValueEnvelope {
        expires_at_ms: u64::from_be_bytes(expires_at_bytes),
        written_at_ms: 0,
        payload: &value[CALYX_KV_VALUE_V1_HEADER_BYTES..],
    })
}

fn decode_calyx_value_v2(value: &[u8]) -> Result<CalyxValueEnvelope<'_>, String> {
    if value.len() < CALYX_KV_VALUE_HEADER_BYTES {
        return Err(format!(
            "Calyx KV v2 value is shorter than its {CALYX_KV_VALUE_HEADER_BYTES} byte header"
        ));
    }
    let mut expires_at_bytes = [0_u8; 8];
    expires_at_bytes.copy_from_slice(&value[1..9]);
    let mut written_at_bytes = [0_u8; 8];
    written_at_bytes.copy_from_slice(&value[9..17]);
    Ok(CalyxValueEnvelope {
        expires_at_ms: u64::from_be_bytes(expires_at_bytes),
        written_at_ms: u64::from_be_bytes(written_at_bytes),
        payload: &value[CALYX_KV_VALUE_HEADER_BYTES..],
    })
}

const fn calyx_value_is_expired(expires_at_ms: u64, now_ms: u64) -> bool {
    expires_at_ms != 0 && now_ms >= expires_at_ms
}

fn calyx_read_failed(
    cf_name: &str,
    action: &'static str,
    source: &SynapseCalyxError,
) -> StorageError {
    tracing::error!(
        code = source.code,
        source_code = source.source_code.unwrap_or("none"),
        remediation = source.remediation,
        cf_name,
        action,
        error = %source,
        "Calyx storage backend read failed"
    );
    StorageError::CalyxReadFailed {
        cf_name: cf_name.to_owned(),
        code: source.code,
        detail: format!("{action}: {source}"),
        // The value was already in hand here — it was being logged and then
        // dropped, so the specific fix reached the operator's log and never the
        // error envelope they actually read (#1911).
        remediation: source.remediation,
    }
}

fn current_action_causal_registry(
    vault: &SynapseCalyxVault,
) -> StorageResult<SynapseCalyxCausalViewRegistryReadback> {
    let scope = SynapseCalyxCausalViewRegistryScope {
        panel_version: SYN_ACTION_PANEL_VERSION,
        corpus_shard: "synapse.action".to_owned(),
        anchor_kind: "reward".to_owned(),
    };
    let readback = vault
        .read_causal_view_registry(&scope)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_registry",
                "read and verify the canonical action causal-view Registry",
                &source,
            )
        })?
        .ok_or_else(|| {
            let source = SynapseCalyxError::new(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_ABSENT",
                "canonical panel=2260001 corpus_shard=synapse.action anchor=reward Registry row is absent",
                "run storage intelligence view_registry_measure explicitly before Oracle validation/readiness/serving",
            );
            calyx_write_failed("calyx_registry", "read canonical action Registry", &source)
        })?;
    let physical = crate::constellations::syn_action_causal_physical_lens_bindings()?;
    let producing = readback
        .registry
        .catalog
        .iter()
        .filter(|view| view.producing)
        .collect::<Vec<_>>();
    let physical_matches = producing.len() == physical.len()
        && producing.iter().all(|view| {
            let Some(slot) = view.slot else {
                return false;
            };
            physical.get(&slot).is_some_and(|binding| {
                view.lens_id.as_deref() == Some(binding.lens_id.as_str())
                    && view.lens_spec_sha256.as_deref() == Some(binding.lens_spec_sha256.as_str())
                    && view.extractor_schema_sha256.as_deref()
                        == Some(binding.extractor_schema_sha256.as_str())
            })
        });
    if !physical_matches {
        let source = SynapseCalyxError::new(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_CONTRACT_MOVED",
            format!(
                "Registry producing contracts={} current physical bindings={} and at least one LensId/LensSpec/extractor digest differs",
                producing.len(),
                physical.len()
            ),
            "allocate a new immutable action panel/view generation and explicitly remeasure the Registry; serving never reinterprets a changed extractor under an old content id",
        );
        return Err(calyx_write_failed(
            "calyx_registry",
            "bind Registry to the current physical action panel",
            &source,
        ));
    }
    Ok(readback)
}

fn calyx_write_failed(cf_name: &str, action: &str, source: &SynapseCalyxError) -> StorageError {
    tracing::error!(
        code = source.code,
        source_code = source.source_code.unwrap_or("none"),
        remediation = source.remediation,
        cf_name,
        action,
        error = %source,
        "Calyx storage backend write failed"
    );
    StorageError::CalyxWriteFailed {
        cf_name: cf_name.to_owned(),
        code: source.code,
        detail: format!("{action}: {source}"),
        remediation: source.remediation,
        committed_seq: None,
    }
}

fn calyx_conditional_write_failed(
    cf_name: &str,
    action: &str,
    source: &SynapseCalyxConditionalWriteError,
) -> StorageError {
    tracing::error!(
        code = source.source.code,
        source_code = source.source.source_code.unwrap_or("none"),
        remediation = source.source.remediation,
        committed_seq = source.committed_seq,
        cf_name,
        action,
        error = %source,
        "revision-guarded Calyx storage write failed"
    );
    StorageError::CalyxWriteFailed {
        cf_name: cf_name.to_owned(),
        code: source.source.code,
        detail: format!("{action}: {}", source.source),
        remediation: source.source.remediation,
        committed_seq: source.committed_seq,
    }
}

fn calyx_write_failed_detail(cf_name: &str, detail: impl Into<String>) -> StorageError {
    let detail = detail.into();
    tracing::error!(
        code = error_codes::STORAGE_WRITE_FAILED,
        cf_name,
        detail,
        "Calyx storage backend write failed"
    );
    StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
        detail,
    }
}

fn revision_guarded_mutation_failed(
    cf_name: &str,
    code: &'static str,
    detail: String,
) -> StorageError {
    tracing::error!(
        code,
        cf_name,
        detail,
        "revision-guarded storage mutation failed"
    );
    StorageError::RevisionGuardedMutationFailed {
        cf_name: cf_name.to_owned(),
        code,
        detail,
    }
}

fn calyx_operation_failed(cf_name: &str, write: bool, detail: String) -> StorageError {
    if write {
        StorageError::WriteFailed {
            cf_name: cf_name.to_owned(),
            detail,
        }
    } else {
        StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail,
        }
    }
}

fn calyx_open_failed(path: &Path, source: &SynapseCalyxError) -> StorageError {
    calyx_open_failed_detail(path, source.to_string())
}

fn calyx_open_failed_detail(path: &Path, detail: String) -> StorageError {
    open_failed_detail_with_backend(path, StorageBackendKind::Calyx, detail)
}

fn open_failed_detail_with_backend(
    path: &Path,
    backend: StorageBackendKind,
    detail: String,
) -> StorageError {
    tracing::warn!(
        code = error_codes::STORAGE_OPEN_FAILED,
        storage_path = %path.display(),
        backend = backend.as_str(),
        %detail,
        "storage open failed"
    );
    StorageError::OpenFailed {
        path: path.to_path_buf(),
        detail,
    }
}

/// The active panel generation a source CF's constellations are measured at.
///
/// Kept as one fail-closed lookup rather than an inline `match` with a `_`
/// fallback: a wrong panel version here computes a different `cx_id` for the
/// same row, so the backfill would write a *second* constellation instead of
/// re-measuring the existing one, and the coverage readback would climb while
/// the old rows stayed stranded (#1965).
/// What one row's anchor carry-forward did, reported rather than assumed.
#[derive(Debug, Clone, Copy, Default)]
struct AnchorCarryForward {
    /// Anchors physically written onto the active generation's `cx_id`.
    anchors_written: u64,
}

struct AnchorCarryBatchTarget {
    row_index: usize,
    source_key: Vec<u8>,
    active_cx_id: CxId,
    prior_anchors: Vec<Anchor>,
}

/// Carries a source row's already-observed grounded anchors across a panel bump.
///
/// **#1980.** `cx_id = hash(input_bytes, panel_version, vault_salt)` and an
/// anchor is keyed by `(cx_id, kind)`, so a panel-version bump re-keys every
/// record and orphans every anchor written against the previous generation. The
/// re-measured corpus is then born ungrounded — measured on this vault as
/// `syn-episode-v1` going from 171-of-171 grounded at `1_904_002` to 0-of-171 at
/// `1_964_001`, off the same 171 source rows.
///
/// That is a silent loss of the one thing the whole intelligence stack is
/// defined against: bits are measured ABOUT anchors, and a panel with no anchor
/// makes every bits/sufficiency/kernel result provisional at best and refused at
/// worst.
///
/// An anchor is an observed real outcome **of the source row** — a label, a
/// reward, a pass/fail — not a property of the lens layout that measured it. The
/// source row is byte-identical across the bump (it is the very thing being
/// re-measured), so the outcome remains true and is carried rather than
/// re-derived. Nothing is invented: only anchors that physically exist on a
/// declared superseded generation of *this* panel, for *this* source row, are
/// copied.
///
/// Written through `put_grounding_anchors`, the same ledger-stamped path a fresh
/// anchor takes, so a carried anchor gets its own provenance entry and there is
/// no second way to create an anchor.
///
/// # Errors
///
/// Returns a storage error when the source CF has no declared generation
/// history, when a superseded generation cannot be read, or when the anchor
/// write or its physical readback fails. It never degrades to "carried nothing":
/// a carry that cannot prove it ran is the failure being fixed.
#[allow(
    clippy::too_many_lines,
    reason = "batch preparation, one atomic write, and per-target physical readback are one integrity boundary whose order must remain locally reviewable"
)]
fn carry_forward_grounded_anchors_batch(
    vault: &SynapseCalyxVault,
    source_cf: &str,
    targets: Vec<AnchorCarryBatchTarget>,
) -> StorageResult<Vec<(usize, AnchorCarryForward)>> {
    if targets.is_empty() {
        return Err(calyx_write_failed_detail(
            "calyx_anchor_carry_forward",
            "anchor carry batch must contain at least one source identity",
        ));
    }
    let superseded = constellations::superseded_panel_versions_for_source_cf(source_cf)?;
    if superseded.is_empty() {
        return Ok(targets
            .into_iter()
            .map(|target| (target.row_index, AnchorCarryForward::default()))
            .collect());
    }
    let panel = constellations::anchor_panel_for_source_cf(source_cf)?;
    let mut seen_cx_ids = BTreeMap::<CxId, Vec<u8>>::new();
    let mut prepared = Vec::<(usize, Vec<u8>, CxId, Vec<Anchor>)>::new();
    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        if let Some(first_source_key) =
            seen_cx_ids.insert(target.active_cx_id, target.source_key.clone())
        {
            return Err(calyx_write_failed_detail(
                "calyx_anchor_carry_forward",
                format!(
                    "two source identities resolved to the same active cx_id in one carry batch: cx_id={} first_source_key_hex={} duplicate_source_key_hex={}; source identity would be ambiguous",
                    target.active_cx_id,
                    constellations::hex_encode(&first_source_key),
                    constellations::hex_encode(&target.source_key)
                ),
            ));
        }

        // Kinds the ACTIVE generation already carries are never overwritten. A
        // freshly derived outcome is the better evidence: it was measured from
        // the row as it stands now, where a carried one is historical.
        let mut carry = Vec::new();
        for anchor in target.prior_anchors {
            if !(anchor.confidence.is_finite() && anchor.confidence > 0.0) {
                continue;
            }
            if vault
                .read_anchor_exact(target.active_cx_id, &anchor.kind)
                .map_err(|error| {
                    calyx_read_failed(
                        "calyx_anchor_carry_forward",
                        "read the active generation's exact anchor before batch carrying",
                        &error,
                    )
                })?
                .is_some()
            {
                continue;
            }
            carry.push(anchor);
        }
        outcomes.push((target.row_index, AnchorCarryForward::default()));
        if !carry.is_empty() {
            prepared.push((
                target.row_index,
                target.source_key,
                target.active_cx_id,
                carry,
            ));
        }
    }
    if prepared.is_empty() {
        return Ok(outcomes);
    }

    let carried_count = prepared
        .iter()
        .map(|(_row_index, _source_key, _cx_id, anchors)| anchors.len() as u64)
        .sum::<u64>();
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema": "synapse_anchor_carry_forward/v2",
        "issue": 1980,
        "source_cf": source_cf,
        "source_row_count": prepared.len(),
        "panel_name": panel.panel_name,
        "to_panel_version": panel.panel_version,
        "from_panel_versions": superseded,
        "anchor_count": carried_count,
    }))
    .map_err(|source| StorageError::EncodeJson {
        type_name: "synapse_anchor_carry_forward_ledger_payload",
        source,
    })?;
    let write = vault
        .put_grounding_anchors_for_many(
            prepared
                .iter()
                .map(|(_row_index, _source_key, cx_id, anchors)| (*cx_id, anchors.clone()))
                .collect(),
            payload,
            "synapse-anchor-carry-forward",
        )
        .map_err(|error| {
            calyx_write_failed(
                "calyx_anchor_carry_forward",
                "batch-carry grounded anchors forward across a panel-version bump",
                &error,
            )
        })?;
    if write.anchor_count as u64 != carried_count
        || write
            .written_anchor_count
            .saturating_add(write.existing_anchor_count)
            != write.anchor_count
    {
        return Err(calyx_write_failed_detail(
            "calyx_anchor_carry_forward",
            format!(
                "anchor carry batch write readback is inconsistent: requested={carried_count} reported={} written={} existing={}",
                write.anchor_count, write.written_anchor_count, write.existing_anchor_count
            ),
        ));
    }
    for (row_index, source_key, active_cx_id, carry) in prepared {
        // Physical readback of the Anchors CF, not the write return value.
        let readback = vault.scan_anchors_for_cx(active_cx_id).map_err(|error| {
            calyx_read_failed(
                "calyx_anchor_carry_forward",
                "read back one target from the carried anchor batch",
                &error,
            )
        })?;
        for anchor in &carry {
            if !readback.iter().any(|row| &row.anchor == anchor) {
                return Err(calyx_write_failed_detail(
                    "calyx_anchor_carry_forward",
                    format!(
                        "carried anchor is absent from the Anchors CF after the batch write: cx_id={active_cx_id} kind={} source_cf={source_cf} source_key_hex={}; the constellation would be reported re-measured while staying ungrounded",
                        synapse_calyx::anchor_kind_label(&anchor.kind),
                        constellations::hex_encode(&source_key),
                    ),
                ));
            }
        }
        let row_carried = carry.len() as u64;
        let outcome = outcomes
            .iter_mut()
            .find(|(candidate_index, _)| *candidate_index == row_index)
            .ok_or_else(|| {
                calyx_write_failed_detail(
                    "calyx_anchor_carry_forward",
                    format!("carry batch lost prepared row_index={row_index}"),
                )
            })?;
        outcome.1.anchors_written = row_carried;
        tracing::info!(
            code = "CALYX_ANCHOR_CARRIED_FORWARD",
            source_cf,
            source_key_hex = %constellations::hex_encode(&source_key),
            panel_name = panel.panel_name,
            to_panel_version = panel.panel_version,
            from_panel_versions = ?superseded,
            cx_id = %active_cx_id,
            anchors_carried = row_carried,
            anchors_present_after = readback.len(),
            batch_anchor_count = carried_count,
            "grounded anchors carried across a panel-version bump in one batch with physical Anchors CF readback"
        );
    }
    Ok(outcomes)
}

fn backfill_panel_version(source_cf: &str) -> StorageResult<u32> {
    match source_cf {
        cf::CF_TIMELINE => Ok(SYN_TIMELINE_PANEL_VERSION),
        cf::CF_EPISODES => Ok(SYN_EPISODE_PANEL_VERSION),
        cf::CF_AGENT_TRANSCRIPTS => Ok(SYN_AGENT_TRANSCRIPT_PANEL_VERSION),
        cf::CF_AGENT_EVENTS => Ok(SYN_AGENT_EVENT_PANEL_VERSION),
        cf::CF_ACTION_LOG => Ok(SYN_ACTION_PANEL_VERSION),
        cf::CF_REFLEX_AUDIT => Ok(SYN_REFLEX_PANEL_VERSION),
        cf::CF_PROCESS_HISTORY => Ok(SYN_PROCESS_PANEL_VERSION),
        cf::CF_OBSERVATIONS => Ok(SYN_OBSERVATION_PANEL_VERSION),
        SYN_MCP_USAGE_BACKFILL_SOURCE => Ok(SYN_MCP_USAGE_PANEL_VERSION),
        SYN_OUTCOME_BACKFILL_SOURCE => Ok(SYN_OUTCOME_PANEL_VERSION),
        other => Err(StorageError::BackendInvalidConfig {
            value: other.to_owned(),
            detail: "no active panel generation is declared for this backfill source CF".to_owned(),
        }),
    }
}

fn backfill_physical_source_cf(source_cf: &str) -> &str {
    if matches!(
        source_cf,
        SYN_MCP_USAGE_BACKFILL_SOURCE | SYN_OUTCOME_BACKFILL_SOURCE
    ) {
        cf::CF_KV
    } else {
        source_cf
    }
}
