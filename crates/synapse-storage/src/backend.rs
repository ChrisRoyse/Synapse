use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use calyx_aster::{
    cf::{ColumnFamily, KeyRange, prefix_range},
    mvcc::{CfRead, Freshness, LATEST_CF_RANGE_PAGE_MAX_ROWS, tombstone_value},
    wal,
};
use calyx_core::{Anchor, AnchorKind, AnchorValue, Constellation, CxId, TemporalPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_calyx::{
    AsterOrphanSlotGcReport, SynapseCalyxAbundanceReport, SynapseCalyxAnchorBatchWriteReadback,
    SynapseCalyxAnchorReadback, SynapseCalyxAnchorWriteReadback, SynapseCalyxAssayParams,
    SynapseCalyxAtomicConstellationRecurrenceReadback, SynapseCalyxBackupReport,
    SynapseCalyxBitsReport, SynapseCalyxBlindSpotParams, SynapseCalyxBlindSpotReport,
    SynapseCalyxCausalityReport, SynapseCalyxCfRangePage, SynapseCalyxCfRows, SynapseCalyxCfWrite,
    SynapseCalyxConditionalWriteError, SynapseCalyxConfig, SynapseCalyxDriftReport,
    SynapseCalyxEnsembleCardReport, SynapseCalyxErasureReport, SynapseCalyxError,
    SynapseCalyxFindParams, SynapseCalyxFindReport, SynapseCalyxGroundedObservationReadback,
    SynapseCalyxGroundingGapReport, SynapseCalyxGuardCalibrateParams,
    SynapseCalyxGuardCalibrateReport, SynapseCalyxGuardVerifyParams, SynapseCalyxGuardVerifyReport,
    SynapseCalyxHazardReport, SynapseCalyxKernelAnswerReport, SynapseCalyxKernelHealthReport,
    SynapseCalyxKernelParams, SynapseCalyxKernelRebuildParams, SynapseCalyxKernelRebuildReport,
    SynapseCalyxKernelReport, SynapseCalyxLedgerEntryReadback, SynapseCalyxLedgerVerifyReport,
    SynapseCalyxMultiConditionalWriteOutcome, SynapseCalyxObservationPutReadback,
    SynapseCalyxPanelDriftParams, SynapseCalyxPanelDriftReport, SynapseCalyxPanelState,
    SynapseCalyxPeriodicityReport, SynapseCalyxPersistedNoveltyFinding,
    SynapseCalyxPersistedRecurrenceFinding, SynapseCalyxPersistedRegionFinding,
    SynapseCalyxReadOnlyVault, SynapseCalyxRecurrenceAppendReadback,
    SynapseCalyxRecurrenceSeriesReadback, SynapseCalyxRedundancyReport,
    SynapseCalyxReproduceReport, SynapseCalyxRetiredSearchGeneration, SynapseCalyxRevisionGuard,
    SynapseCalyxSearchRebuildReport, SynapseCalyxSufficiencyReport, SynapseCalyxTemporalCandidate,
    SynapseCalyxTemporalParams, SynapseCalyxTemporalRerankReadback, SynapseCalyxVault,
    SynapseCalyxVaultCloseReadback, SynapseCalyxVaultStatus, SynapseCalyxVaultVerifyReport,
    SynapseCalyxVerifyReport, SynapseCalyxWeaveParams, SynapseCalyxWeaveReport,
    VaultTemporalPanelRegistration,
};
use synapse_core::{
    error_codes,
    retention::{DEFAULTS, RetentionDefault, RetentionTtl},
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
    SYN_OUTCOME_BACKFILL_SOURCE, SYN_OUTCOME_KEY_PREFIX, SYN_OUTCOME_PANEL_NAME,
    SYN_OUTCOME_PANEL_VERSION, SYN_PROCESS_PANEL_NAME, SYN_PROCESS_PANEL_VERSION,
    SYN_RECURRENCE_SUBJECT_PANEL_NAME, SYN_RECURRENCE_SUBJECT_PANEL_VERSION, SYN_REFLEX_PANEL_NAME,
    SYN_REFLEX_PANEL_VERSION, SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION,
    SupersededPanelLineage, assert_syn_lens_provenance_complete, superseded_panel_lineage,
    syn_active_panel_contract,
};
use crate::{
    CfEstimateMap, CfRevisionGuard, CoherentScanLease, CoherentScanScope, FixedWidthScanPage,
    OwnedCfWriteBatch, PhysicalScanPage, RawRow, RevisionGuard, RevisionGuardConflict,
    RevisionGuardedMutationOutcome, RevisionGuardedWriteOutcome, RevisionedRawValue,
    STORAGE_REVISION_GUARD_INVALID, STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE,
    STORAGE_REVISION_GUARDED_OUTCOME_INVALID, ScanWindow, StorageError, StorageResult, cf,
    constellations, gc, pressure,
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

struct EpisodeConstellationBatch {
    constellations: Vec<Constellation>,
    pending_reports: Vec<PendingConstellationReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalyxVaultInspect {
    pub schema_version: u32,
    pub vault_id: String,
    pub latest_seq: u64,
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
    fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>>;
    fn get_cf_revisioned(
        &self,
        cf_name: &str,
        key: &[u8],
    ) -> StorageResult<Option<RevisionedRawValue>>;
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
    fn oracle_predict_action(&self, action_id: &str) -> StorageResult<Value>;
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
        source_key: Option<&[u8]>,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<constellations::TemporalMetadataBackfillReport>;
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
    fn put_agent_transcript_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &AgentTranscriptRecord,
    ) -> StorageResult<ConstellationPutReport>;
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
    pressure: Arc<pressure::PressureState>,
    anchor_carry_lineage: Mutex<Option<AnchorCarryLineageCache>>,
}

struct AnchorCarryLineageCache {
    source_cf: String,
    superseded_versions: Vec<u32>,
    by_source_key: Arc<BTreeMap<String, Vec<Anchor>>>,
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

    fn oracle_predict_action(&self, action_id: &str) -> StorageResult<Value> {
        let action_id = validate_recurrence_subject_id(action_id)?;
        self.with_vault(
            "calyx_oracle",
            "predict terminal action outcome",
            true,
            |vault| {
                let created_at_ms = calyx_clock_now_for_write(vault, "calyx_oracle")?;
                let mut panel = syn_active_panel_contract(SYN_ACTION_PANEL_VERSION, created_at_ms)?
                    .ok_or_else(|| {
                        calyx_write_failed_detail(
                            "calyx_oracle",
                            "syn-action panel contract is absent",
                        )
                    })?
                    .panel;
                let assay = synapse_calyx::SynapseCalyxAssayParams::new(
                    SYN_ACTION_PANEL_VERSION,
                    "reward".to_owned(),
                )
                .with_corpus_shard("synapse.action".to_owned())
                .with_lens_names(crate::constellations::syn_slot_lens_names());
                let capability = vault.assay_ensemble_card(&assay, 2).map_err(|source| {
                    calyx_write_failed(
                        "calyx_oracle",
                        "measure calibrated action-panel capability before prediction",
                        &source,
                    )
                })?;
                panel
                    .slots
                    .retain(|slot| capability.measured_slots.contains(&slot.slot_id.get()));
                if panel.slots.is_empty() {
                    return Err(calyx_write_failed(
                        "calyx_oracle",
                        "measure action-panel sufficiency before prediction",
                        &SynapseCalyxError::new(
                            "CALYX_ORACLE_INSUFFICIENT",
                            "action-panel sufficiency produced no measured slots",
                            "anchor at least 50 diverse terminal outcomes before prediction",
                        ),
                    ));
                }
                vault
                    .oracle_predict_action(&action_id, "synapse.action", panel)
                    .map_err(|source| {
                        calyx_write_failed("calyx_oracle", "predict action outcome", &source)
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
                    .oracle_reverse_action(AnchorValue::Bool(outcome), "synapse.action")
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
            let mut panel = syn_active_panel_contract(SYN_ACTION_PANEL_VERSION, created_at_ms)?
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
            let assay = synapse_calyx::SynapseCalyxAssayParams::new(
                SYN_ACTION_PANEL_VERSION,
                "reward".to_owned(),
            )
            .with_corpus_shard("synapse.action".to_owned())
            .with_lens_names(crate::constellations::syn_slot_lens_names());
            let capability = vault.assay_ensemble_card(&assay, 2).map_err(|source| {
                calyx_write_failed(
                    "calyx_oracle",
                    "measure calibrated action-panel capability before completion",
                    &source,
                )
            })?;
            panel
                .slots
                .retain(|slot| capability.measured_slots.contains(&slot.slot_id.get()));
            if panel.slots.is_empty() {
                return Err(calyx_write_failed_detail(
                    "calyx_oracle",
                    "action-panel sufficiency produced no measured slots; anchor at least 50 diverse terminal outcomes before completion",
                ));
            }
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
            let mut panel = syn_active_panel_contract(SYN_ACTION_PANEL_VERSION, created_at_ms)?
                .ok_or_else(|| calyx_write_failed_detail("calyx_oracle", "syn-action panel contract is absent"))?
                .panel;
            let assay = synapse_calyx::SynapseCalyxAssayParams::new(
                SYN_ACTION_PANEL_VERSION,
                "reward".to_owned(),
            )
            .with_corpus_shard("synapse.action".to_owned())
            .with_lens_names(crate::constellations::syn_slot_lens_names());
            let capability = vault.assay_ensemble_card(&assay, 2).map_err(|source| {
                calyx_write_failed(
                    "calyx_oracle",
                    "measure calibrated action-panel capability before readiness",
                    &source,
                )
            })?;
            panel
                .slots
                .retain(|slot| capability.measured_slots.contains(&slot.slot_id.get()));
            if panel.slots.is_empty() {
                return Err(calyx_write_failed_detail(
                    "calyx_oracle",
                    "action-panel sufficiency produced no measured slots; anchor at least 50 diverse terminal outcomes before readiness",
                ));
            }
            let snapshot = vault.measure_action_readiness(&panel).map_err(|source| {
                calyx_write_failed("calyx_oracle", "measure action readiness", &source)
            })?;
            serde_json::to_value(snapshot).map_err(|error| {
                calyx_write_failed_detail("calyx_oracle", format!("encode readiness snapshot: {error}"))
            })
        })
    }

    fn oracle_validate_action(&self) -> StorageResult<Value> {
        self.with_vault(
            "calyx_oracle",
            "validate action readiness evidence",
            true,
            |vault| {
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
                    calyx_operation_failed("calyx_oracle", false, source.to_string())
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

    fn close(&self, reason: &'static str) -> StorageResult<SynapseCalyxVaultCloseReadback> {
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
        vault.close(reason).map_err(|source| {
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
            return Ok(Arc::new(BTreeMap::new()));
        }
        if !reset_for_new_sweep {
            let guard = self
                .anchor_carry_lineage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cache) = guard.as_ref().filter(|cache| {
                cache.source_cf == source_cf && cache.superseded_versions == superseded
            }) {
                return Ok(Arc::clone(&cache.by_source_key));
            }
        }

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
        let by_source_key = Arc::new(by_source_key);
        let mut guard = self
            .anchor_carry_lineage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(AnchorCarryLineageCache {
            source_cf: source_cf.to_owned(),
            superseded_versions: superseded.to_vec(),
            by_source_key: Arc::clone(&by_source_key),
        });
        Ok(by_source_key)
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
        // Coherent multi-page scans pin an MVCC sequence while writers continue
        // advancing the vault. The writable backend must therefore restore
        // historical rows; latest-only recovery cannot honor those leases.
        let vault = match SynapseCalyxVault::open(config.clone()) {
            Ok(vault) => vault,
            Err(source) if source.source_code == Some("CALYX_ASTER_ROUTER_ONLY_ROWS") => {
                tracing::warn!(
                    code = "STORAGE_CALYX_ROUTER_ONLY_ROWS_ADOPTION_START",
                    storage_path = %path.display(),
                    detail = %source.message,
                    "adopting legacy router-only rows into the commit domain before retrying the required full-MVCC open"
                );
                let adoption = SynapseCalyxVault::open_latest_readback(config.clone())
                    .map_err(|adoption_error| calyx_open_failed(path, &adoption_error))?;
                adoption
                    .purge_kv_tombstones()
                    .map_err(|adoption_error| calyx_open_failed(path, &adoption_error))?;
                let close = adoption
                    .close("router_only_row_adoption")
                    .map_err(|adoption_error| calyx_open_failed(path, &adoption_error))?;
                if !close.safe_to_unlock {
                    return Err(StorageError::OpenFailed {
                        path: path.to_path_buf(),
                        detail: "CALYX_ROUTER_ONLY_ROW_ADOPTION_CLOSE_UNSAFE: adoption vault did not prove safe lock release before full-MVCC reopen".to_owned(),
                    });
                }
                let reopened = SynapseCalyxVault::open(config)
                    .map_err(|reopen_error| calyx_open_failed(path, &reopen_error))?;
                tracing::info!(
                    code = "STORAGE_CALYX_ROUTER_ONLY_ROWS_ADOPTION_DONE",
                    storage_path = %path.display(),
                    latest_seq = reopened.latest_seq(),
                    "adopted router-only rows and proved the required full-MVCC reopen"
                );
                reopened
            }
            Err(source) => return Err(calyx_open_failed(path, &source)),
        };
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
        Ok(Self {
            path: path.to_path_buf(),
            vault: Arc::new(CalyxVaultRuntime::new(vault)),
            pressure: Arc::new(pressure::PressureState::default()),
            anchor_carry_lineage: Mutex::new(None),
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

    /// Grounds one already-measured transcript row, reporting whether the row
    /// carried an adjudicated outcome at all (#1926).
    ///
    /// `Ok(None)` means "this row has nothing to adjudicate", which is a real
    /// answer for the ~74% of transcript rows that are not observed tool
    /// results. It is never used to swallow a failed write.
    fn put_agent_transcript_outcome_anchor_row(
        &self,
        source_key: &[u8],
        source_value: &[u8],
        record: &AgentTranscriptRecord,
    ) -> StorageResult<Option<()>> {
        let Some(anchor) = constellations::agent_transcript_outcome_anchor(record) else {
            return Ok(None);
        };
        let panel =
            constellations::anchor_panel_for_source_row(cf::CF_AGENT_TRANSCRIPTS, source_key)?;
        let input_bytes = constellations::source_constellation_input_bytes(
            panel.input_mode,
            cf::CF_AGENT_TRANSCRIPTS,
            source_key,
            source_value,
        );
        let calyx_anchor = grounding_anchor_to_calyx(anchor.clone())?;
        let existing = self.with_vault(
            "calyx_agent_transcript_outcome_anchor",
            "read exact current transcript outcome before anchoring",
            false,
            |vault| {
                let cx_id = vault.cx_id_for_input(&input_bytes, panel.panel_version);
                vault
                    .read_anchor_exact(cx_id, &calyx_anchor.kind)
                    .map_err(|error| {
                        calyx_read_failed(
                            "calyx_agent_transcript_outcome_anchor",
                            "read exact current transcript outcome before anchoring",
                            &error,
                        )
                    })
            },
        )?;
        if let Some(existing) = existing {
            if existing.anchor != calyx_anchor {
                return Err(calyx_write_failed_detail(
                    "calyx_agent_transcript_outcome_anchor",
                    format!(
                        "active transcript anchor kind already exists with conflicting evidence: source_key_hex={} panel_version={} kind={}; immutable grounded outcomes cannot be overwritten",
                        constellations::hex_encode(source_key),
                        panel.panel_version,
                        synapse_calyx::anchor_kind_label(&calyx_anchor.kind),
                    ),
                ));
            }
            return Ok(Some(()));
        }
        let payload = constellations::grounding_anchor_ledger_payload(
            cf::CF_AGENT_TRANSCRIPTS,
            source_key,
            source_value,
            &anchor,
        );
        <Self as StorageBackend>::put_grounding_anchor_for_source(
            self,
            cf::CF_AGENT_TRANSCRIPTS,
            source_key,
            source_value,
            anchor,
            &payload,
        )?;
        Ok(Some(()))
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
    let Some(contract) = syn_active_panel_contract(SYN_TIMELINE_PANEL_VERSION, created_at_ms)?
    else {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_manifest".to_owned(),
            detail: format!(
                "STORAGE_CALYX_ACTIVE_PANEL_CONTRACT_MISSING: no enumerated content-slot contract for primary panel generation {SYN_TIMELINE_PANEL_VERSION}; remediation=add the panel's slot contract to syn_active_panel_contract before publication"
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
            if syn_active_panel_contract(panel_version, created_at_ms)?.is_some() {
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

fn ensure_builtin_panel_generation_reservations(vault: &SynapseCalyxVault) -> StorageResult<()> {
    let reservations = [
        (SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION),
        (SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION),
        (SYN_AGENT_EVENT_PANEL_NAME, SYN_AGENT_EVENT_PANEL_VERSION),
        (
            SYN_AGENT_TRANSCRIPT_PANEL_NAME,
            SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        ),
        (SYN_ACTION_PANEL_NAME, SYN_ACTION_PANEL_VERSION),
        (SYN_REFLEX_PANEL_NAME, SYN_REFLEX_PANEL_VERSION),
        (SYN_PROCESS_PANEL_NAME, SYN_PROCESS_PANEL_VERSION),
        (SYN_OBSERVATION_PANEL_NAME, SYN_OBSERVATION_PANEL_VERSION),
        (SYN_OUTCOME_PANEL_NAME, SYN_OUTCOME_PANEL_VERSION),
        (SYN_MCP_USAGE_PANEL_NAME, SYN_MCP_USAGE_PANEL_VERSION),
        (
            SYN_RECURRENCE_SUBJECT_PANEL_NAME,
            SYN_RECURRENCE_SUBJECT_PANEL_VERSION,
        ),
    ]
    .map(|(name, generation)| (name.to_owned(), generation));
    let readback = vault
        .reserve_panel_generations(&reservations)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_registry",
                "reserve built-in native Calyx panel generations",
                &source,
            )
        })?;
    if readback.owner_count < reservations.len() as u64
        || readback.next_generation <= SYN_MCP_USAGE_PANEL_VERSION
    {
        return Err(StorageError::WriteFailed {
            cf_name: "calyx_registry".to_owned(),
            detail: format!(
                "STORAGE_CALYX_PANEL_GENERATION_READBACK_INVALID: owners={} expected_at_least={} next={} required_above={}; remediation=inspect native Registry CF allocator ownership before any panel lifecycle mutation",
                readback.owner_count,
                reservations.len(),
                readback.next_generation,
                SYN_MCP_USAGE_PANEL_VERSION
            ),
        });
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

struct CalyxPressureMaintenance {
    vault: Arc<CalyxVaultRuntime>,
}

impl CalyxPressureMaintenance {
    const fn new(vault: Arc<CalyxVaultRuntime>) -> Self {
        Self { vault }
    }
}

impl pressure::PressureMaintenance for CalyxPressureMaintenance {
    fn compact_for_pressure(&self) -> StorageResult<Vec<&'static str>> {
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
struct CalyxDerivedStateRunner;

impl gc::GcRunner for CalyxDerivedStateRunner {
    fn run_once(&self) -> StorageResult<gc::GcReport> {
        crate::derived_state::run_derived_state_maintenance();
        Ok(gc::GcReport::default())
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
        self.vault.with_vault(
            CALYX_CHECKPOINT_CF,
            "periodic durable checkpoint",
            true,
            |vault| {
                vault.checkpoint().map_err(|source| {
                    calyx_write_failed(
                        CALYX_CHECKPOINT_CF,
                        "materialize staged durable checkpoints and advance manifest floor",
                        &source,
                    )
                })
            },
        )?;
        Ok(gc::GcReport::default())
    }
}

struct CalyxGcRunner {
    vault: Arc<CalyxVaultRuntime>,
}

impl CalyxGcRunner {
    const fn new(vault: Arc<CalyxVaultRuntime>) -> Self {
        Self { vault }
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
        self.run_with_budgets(std::slice::from_ref(&budget))
    }

    fn run_with_budgets(&self, budgets: &[CalyxGcBudget]) -> StorageResult<gc::GcReport> {
        self.vault
            .with_vault(CALYX_GC_CF, "run Calyx GC", true, |vault| {
                run_calyx_gc_budgets(vault, budgets)
            })
    }
}

impl gc::GcRunner for CalyxGcRunner {
    fn run_once(&self) -> StorageResult<gc::GcReport> {
        self.run_default_once()
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

fn lifecycle_constellation_from_source(
    vault: &SynapseCalyxVault,
    panel_name: &str,
    source_panel_version: u32,
    source_cf: &str,
    source_key: &[u8],
    raw: &[u8],
) -> StorageResult<Constellation> {
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
    vault
        .materialize_panel_lifecycle_generation(panel_name, &identity, raw, &base)
        .map_err(|source| {
            calyx_write_failed(
                "calyx_constellation",
                "materialize lifecycle backfill generation from authoritative source",
                &source,
            )
        })?
        .ok_or_else(|| {
            calyx_write_failed_detail(
                "calyx_registry",
                format!("panel {panel_name} lifecycle row disappeared after its task was claimed"),
            )
        })
}

fn resolve_panel_contract(
    vault: &SynapseCalyxVault,
    panel_version: u32,
    created_at_ms: u64,
) -> StorageResult<Option<SynapseCalyxPanelState>> {
    if let Some(contract) = syn_active_panel_contract(panel_version, created_at_ms)? {
        return Ok(Some(SynapseCalyxPanelState {
            panel: contract.panel,
            registry: contract.registry,
            registry_snapshot: None,
        }));
    }
    let mut matched = None;
    for entry in constellations::builtin_panel_catalog() {
        let base_registry = match syn_active_panel_contract(entry.panel_version, created_at_ms)? {
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
                calyx_write_failed(
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
        if matched.is_some() {
            return Err(calyx_write_failed_detail(
                "calyx_registry",
                format!(
                    "multiple durable lifecycle panels claim generation {panel_version}; generation identity is ambiguous"
                ),
            ));
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
                }))
            })
        })
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

    fn run_gc_once(&self) -> StorageResult<gc::GcReport> {
        CalyxGcRunner::new(Arc::clone(&self.vault)).run_default_once()
    }

    fn run_gc_once_with_row_caps(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<gc::GcReport> {
        CalyxGcRunner::new(Arc::clone(&self.vault)).run_row_cap_once(
            cf_name,
            soft_cap_rows,
            hard_cap_rows,
        )
    }

    fn spawn_gc_task(&self) -> StorageResult<gc::GcTask> {
        let config = gc::GcConfig::from_retention_defaults();
        gc::spawn_runner(
            Arc::new(CalyxGcRunner::new(Arc::clone(&self.vault))),
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
            Arc::new(CalyxDerivedStateRunner),
            crate::derived_state::DERIVED_STATE_INTERVAL,
            gc::MaintenanceTaskKind::DerivedState,
        )
    }

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
            let materialized = self.with_vault(
                "calyx_constellation",
                "remeasure lifecycle backfill source row",
                true,
                |vault| {
                    let expected = lifecycle_constellation_from_source(
                        vault,
                        entry.panel_name,
                        panel_version,
                        &source_cf,
                        &source_key,
                        &raw,
                    )?;
                    vault
                        .put_observation_constellation(expected.clone())
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put lifecycle backfill constellation",
                                &source,
                            )
                        })?;
                    let observed = vault
                        .hydrate_constellation_latest(expected.cx_id)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "independently hydrate lifecycle backfill constellation",
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
                                "physical lifecycle readback differs for expected cx_id {} panel {}",
                                expected.cx_id, expected.panel_version
                            ),
                        ));
                    }
                    Ok(expected.cx_id)
                },
            )?;
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
                let mut targets = published.panels.clone();
                if let Some(active) = active_panel_version
                    && !targets.contains(&active)
                {
                    targets.push(active);
                }
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

                let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
                // Genuinely pass-level, so it is checked once here rather than
                // once per panel inside `syn_active_panel_contract`. An
                // undeclared lens provenance is a property of the *code*, not of
                // any one generation, and attributing it to whichever panel the
                // loop happened to reach first would name the wrong culprit
                // (#1971 finding 2).
                assert_syn_lens_provenance_complete()?;
                let mut generations = Vec::with_capacity(targets.len());
                for panel_version in targets {
                    let is_active_panel = active_panel_version == Some(panel_version);
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
                    let supplied = match syn_active_panel_contract(panel_version, created_at_ms) {
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
                        let disposition = match superseded_panel_lineage(panel_version) {
                            Some(lineage) => {
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
                            }
                            None => {
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
                            }
                        };
                        generations.push(PanelGenerationMaintenance {
                            panel_version,
                            is_active_panel,
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
                        disposition,
                    });
                }

                let sweep = SearchGenerationSweep {
                    index_root: published.index_root.display().to_string(),
                    active_panel_version,
                    generations,
                    unrecognized_index_entries: published.unrecognized,
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                };
                tracing::info!(
                    code = "STORAGE_SEARCH_GENERATION_SWEEP_COMPLETED",
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
        // The panels Synapse registers and writes rows to. Measured together so
        // one panel losing its lens layer is visible next to the panels that
        // still carry theirs (#1894).
        const PANELS: [u32; 4] = [
            SYN_TIMELINE_PANEL_VERSION,
            SYN_EPISODE_PANEL_VERSION,
            SYN_AGENT_EVENT_PANEL_VERSION,
            SYN_AGENT_TRANSCRIPT_PANEL_VERSION,
        ];
        self.vault.with_vault(
            "calyx_lens_coverage",
            "measure Calyx panel lens coverage",
            true,
            |vault| {
                vault
                    .lens_coverage_status(&PANELS, max_records)
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
        let census = self.vault.with_vault(
            "calyx_panel_census",
            "census every Calyx panel generation in the Base CF",
            true,
            |vault| {
                vault.panel_census().map_err(|source| {
                    calyx_write_failed(
                        "calyx_panel_census",
                        "census every Calyx panel generation in the Base CF",
                        &source,
                    )
                })
            },
        )?;

        // Count ONLY the CFs that are a declared full-CF denominator. Counting
        // every CF would read ~28k unrelated CF_KV rows for a number the report
        // deliberately does not derive a fraction from (a subset-fed panel has
        // no meaningful denominator), and this pass runs on a five-minute tick.
        let mut source_cf_rows = BTreeMap::new();
        // #1940: the KEYS, not just the count. The orphan question — "does this
        // record's own source row still exist" — is a membership test, and the
        // rows are already in hand here, so keeping their keys costs one pass
        // over an array that was going to be dropped anyway.
        let mut source_cf_keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entry in constellations::builtin_panel_catalog() {
            let Some(cf_name) = entry.source.cf_name() else {
                continue;
            };
            if source_cf_keys.contains_key(cf_name) {
                continue;
            }
            // The row COUNT is still only taken for a declared full-CF
            // denominator: a subset-fed panel has no meaningful coverage
            // fraction, and deriving one would read as a permanent outage.
            //
            // The KEY SET is taken for every source CF, including subset-fed
            // ones (#1940). The orphan probe is a per-record membership test —
            // "is this record's own source row still there" — and that question
            // is exactly as meaningful for a sampled panel as for a full-CF
            // one. Skipping them made the census silently under-report:
            // measured 2026-08-01 it found 226 of the 227 orphans an
            // independent audit found, missing `syn-observation-v1`'s one
            // record purely because its panel is sampled.
            if entry.source.is_full_cf() {
                let rows = self.read_all_rows(cf_name)?;
                source_cf_rows.insert(cf_name.to_owned(), rows.len() as u64);
            }
            // The KEY SET is read with expired rows INCLUDED, and that is not
            // the same question the row count above answers (#1940).
            //
            // `source_cf_rows` is a coverage denominator: "how many source rows
            // is this panel supposed to have measured". A row past its TTL is
            // on its way out and measuring it is pointless, so it is excluded.
            //
            // The orphan probe asks something else: "can this record's own
            // source row still be read". A row past its TTL but still
            // physically in the vault CAN be read and re-measured until the GC
            // actually removes it, so counting it as gone would overstate the
            // loss — the same call `--audit-source-coverage` makes for the same
            // reason (#1882). Measured on the live vault 2026-08-01, the two
            // readings of CF_ACTION_LOG differ by a factor of nine (82 unexpired
            // vs 747 physically present), so this is the difference between
            // reporting 665 orphans and the true 224.
            let present = self.with_vault(cf_name, "scan Calyx KV namespace", false, |vault| {
                read_all_rows_from_vault_including_expired(vault, cf_name)
            })?;
            source_cf_keys.insert(
                cf_name.to_owned(),
                present
                    .iter()
                    .map(|(key, _)| constellations::hex_encode(key))
                    .collect(),
            );
        }

        let report = crate::panel_coverage::build_panel_coverage_report(
            &census,
            &source_cf_rows,
            &source_cf_keys,
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
                    resolve_panel_contract(vault, expected_panel_version, created_at_ms)?;
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
                let panel = resolve_panel_contract(vault, expected_panel_version, created_at_ms)?
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
                // reconstruct it — `syn_active_panel_contract` lives here, in the
                // crate that depends on it — so the contract is resolved on this
                // side and handed down.
                //
                // `None` (no panel named) and an unknown version both hand down
                // no contract: the first queries the active panel exactly as
                // before, the second fails closed naming both lookups rather
                // than searching a panel whose slots were never validated.
                let supplied = match params.panel_version {
                    Some(version) => {
                        let created_at_ms = calyx_clock_now_for_write(vault, "calyx_manifest")?;
                        resolve_panel_contract(vault, version, created_at_ms)?
                    }
                    None => None,
                };
                vault
                    .find_similar_in_panel(params, supplied.as_ref())
                    .map_err(|source| {
                        calyx_write_failed(
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
        self.vault.close(reason)
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
            calyx_write_failed("<calyx-vault>", "verify restored Calyx vault", &source)
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

    fn oracle_predict_action(&self, action_id: &str) -> StorageResult<Value> {
        self.vault.oracle_predict_action(action_id)
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
                let contract = syn_active_panel_contract(panel_version, now)?.ok_or_else(|| {
                    StorageError::BackendInvalidConfig {
                        value: panel_version.to_string(),
                        detail: "the active panel has no reconstructable built-in contract; declare every slot runtime in syn_active_panel_contract before mutating it".to_owned(),
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
                    .add_panel_lens(synapse_calyx::panel_lifecycle::SynapseCalyxAddLensRequest {
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
                        synapse_calyx::panel_lifecycle::SynapseCalyxSetLensStateRequest {
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

    fn temporal_periodicity_intelligence(
        &self,
        params: &SynapseCalyxTemporalParams,
    ) -> StorageResult<SynapseCalyxPeriodicityReport> {
        self.with_vault(
            "calyx_assay",
            "measure native Calyx temporal periodicity",
            true,
            |vault| {
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
                        syn_active_panel_contract(params.panel_version, created_at_ms)?
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
        source_key: Option<&[u8]>,
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
        if source_key.is_some() && after_physical.is_some() {
            return Err(StorageError::BackendInvalidConfig {
                value: source_cf.to_owned(),
                detail: "exact source_key and physical page cursor are mutually exclusive"
                    .to_owned(),
            });
        }
        let (rows, resume_after_physical, more, candidate_rows_examined, expired_rows_skipped) =
            if let Some(key) = source_key {
                if source_cf == SYN_MCP_USAGE_BACKFILL_SOURCE
                    && !key.starts_with(SYN_MCP_USAGE_KEY_PREFIX)
                {
                    return Err(StorageError::BackendInvalidConfig {
                        value: constellations::hex_encode(key),
                        detail: "exact MCP-usage backfill key is outside mcp-usage/v1/; remediation=pass a key from the declared prefix"
                            .to_owned(),
                    });
                }
                if source_cf == SYN_OUTCOME_BACKFILL_SOURCE
                    && !key.starts_with(SYN_OUTCOME_KEY_PREFIX)
                {
                    return Err(StorageError::BackendInvalidConfig {
                        value: constellations::hex_encode(key),
                        detail: "exact outcome backfill key is outside escalation/v1/audit/; remediation=pass a key from the declared prefix"
                            .to_owned(),
                    });
                }
                let physical_source_cf = backfill_physical_source_cf(source_cf);
                self.get_cf(physical_source_cf, key)?
                    .map(|value| (vec![(key.to_vec(), value)], None, false, 1, 0))
                    .ok_or_else(|| StorageError::ReadFailed {
                        cf_name: source_cf.to_owned(),
                        detail: format!(
                            "temporal metadata backfill source row not found: key_hex={}",
                            constellations::hex_encode(key)
                        ),
                    })?
            } else {
                let prefix = match source_cf {
                    SYN_MCP_USAGE_BACKFILL_SOURCE => Some(SYN_MCP_USAGE_KEY_PREFIX),
                    SYN_OUTCOME_BACKFILL_SOURCE => Some(SYN_OUTCOME_KEY_PREFIX),
                    _ => None,
                };
                let page = if let Some(prefix) = prefix {
                    self.with_vault(
                        cf::CF_KV,
                        "scan candidate-bounded MCP-usage prefix page",
                        false,
                        |vault| {
                            read_physical_prefix_page_from_vault(
                                vault,
                                cf::CF_KV,
                                prefix,
                                after_physical,
                                max_rows,
                            )
                        },
                    )?
                } else {
                    self.scan_cf_physical_page(source_cf, after_physical, max_rows)?
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
                } else if source_key.is_some() {
                    return Err(StorageError::BackendInvalidConfig {
                        value: constellations::hex_encode(&row.0),
                        detail: "exact CF_OBSERVATIONS row is outside the deterministic sampled panel population"
                            .to_owned(),
                    });
                }
            }
            sampled
        } else {
            rows
        };
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
            self.anchor_carry_lineage(source_cf, source_key.is_none() && after_physical.is_none())?
        } else {
            Arc::new(BTreeMap::new())
        };
        let superseded_generation_count = if carry_superseded_anchors {
            constellations::superseded_panel_versions_for_source_cf(source_cf)?.len() as u64
        } else {
            0
        };
        let mut carry_targets = Vec::with_capacity(rows.len());
        for (key, raw) in rows {
            let identity_input = if source_cf == SYN_MCP_USAGE_BACKFILL_SOURCE {
                constellations::mcp_usage_constellation_input_bytes(cf::CF_KV, &key, &raw)
            } else if source_cf == SYN_OUTCOME_BACKFILL_SOURCE {
                constellations::outcome_constellation_input_bytes(cf::CF_KV, &key, &raw)
            } else {
                raw.clone()
            };
            let disposition = self.with_vault(
                "calyx_temporal_metadata_backfill",
                "backfill Calyx temporal metadata",
                true,
                |vault| {
                    let context = NativeConstellationContext {
                        vault_id: vault.vault_id_value(),
                        cx_id: vault.cx_id_for_input(&identity_input, active_panel_version),
                        created_at_ms: calyx_clock_now_for_write(
                            vault,
                            backfill_physical_source_cf(source_cf),
                        )?,
                        next_ledger_seq: vault.latest_seq().saturating_add(1),
                    };
                    let decode_failed = |error: &serde_json::Error, label: &str| {
                        StorageError::ReadFailed {
                            cf_name: source_cf.to_owned(),
                            detail: format!(
                                "decode authoritative {label} row key_hex={}: {error}",
                                constellations::hex_encode(&key)
                            ),
                        }
                    };
                    let expected = match source_cf {
                        cf::CF_TIMELINE => {
                            let record: TimelineRecord = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "timeline"))?;
                            constellations::build_timeline_constellation(
                                context, &key, &raw, &record,
                            )?
                        }
                        cf::CF_EPISODES => {
                            let record: EpisodeRecord = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "episode"))?;
                            constellations::build_episode_constellation(context, &key, &raw, &record)?
                        }
                        // #1904 gave the agent-transcript panel its first
                        // lexical lane, which needs the same re-measure path the
                        // timeline panel used. The panel is spawn-keyed and by
                        // far the largest on the vault, so it is only ever
                        // reached through the paginated cursor.
                        cf::CF_AGENT_TRANSCRIPTS => {
                            let record: AgentTranscriptRecord = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "agent transcript"))?;
                            constellations::build_agent_transcript_constellation(
                                context, &key, &raw, &record,
                            )?
                        }
                        cf::CF_AGENT_EVENTS => {
                            let record: AgentEventRecord = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "agent event"))?;
                            constellations::build_agent_event_constellation(
                                context, &key, &raw, &record,
                            )?
                        }
                        cf::CF_ACTION_LOG => {
                            let record: Value = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "action"))?;
                            constellations::build_action_constellation(context, &key, &raw, &record)?
                        }
                        cf::CF_REFLEX_AUDIT => {
                            let record: StoredReflexAudit = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "reflex audit"))?;
                            constellations::build_reflex_audit_constellation(
                                context, &key, &raw, &record,
                            )?
                        }
                        cf::CF_OBSERVATIONS => {
                            let record: StoredObservation = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "observation"))?;
                            constellations::build_observation_constellation(
                                context, &key, &raw, &record,
                            )?
                        }
                        SYN_MCP_USAGE_BACKFILL_SOURCE => {
                            let record: Value = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "MCP usage"))?;
                            constellations::build_mcp_usage_constellation(
                                context,
                                &key,
                                &raw,
                                &identity_input,
                                &record,
                            )?
                        }
                        SYN_OUTCOME_BACKFILL_SOURCE => {
                            let record: Value = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "outcome"))?;
                            constellations::build_outcome_constellation(
                                context,
                                cf::CF_KV,
                                &key,
                                &raw,
                                &identity_input,
                                &record,
                            )?
                        }
                        // Exhaustive over the guard above, so a CF added there
                        // without a builder here is a compile-visible omission
                        // rather than a silent mis-measurement.
                        _ => {
                            let record: Value = serde_json::from_slice(&raw)
                                .map_err(|error| decode_failed(&error, "process"))?;
                            constellations::build_process_constellation(
                                context, &key, &raw, &record,
                            )?
                        }
                    };
                    let put = vault
                        .put_observation_constellation(expected.clone())
                        .map_err(|error| {
                            calyx_write_failed(
                                "calyx_temporal_metadata_backfill",
                                "materialize missing Calyx constellation before temporal metadata backfill",
                                &error,
                            )
                        })?;
                    let (identity, temporal) =
                        constellations::temporal_migration_metadata(&expected);
                    let migration = if !constellations::temporal_migration_eligible(&temporal)? {
                        None
                    } else {
                        Some(
                            vault
                                .backfill_temporal_metadata(
                                    expected.cx_id,
                                    expected.panel_version,
                                    &identity,
                                    &temporal,
                                )
                                .map_err(|error| {
                                    calyx_write_failed(
                                        "calyx_temporal_metadata_backfill",
                                        "backfill Calyx temporal metadata",
                                        &error,
                                    )
                                })?,
                        )
                    };
                    Ok((put.disposition, migration, expected.cx_id))
                },
            )?;
            if disposition.1.is_none() {
                temporal_ineligible_rows = temporal_ineligible_rows.saturating_add(1);
            }
            if disposition.0.inserted() {
                inserted_rows = inserted_rows.saturating_add(1);
            } else if disposition.1.as_ref().is_some_and(|value| value.changed()) {
                backfilled_rows = backfilled_rows.saturating_add(1);
            } else if disposition.1.is_some() {
                already_current_rows = already_current_rows.saturating_add(1);
            }

            // #1926: the constellation for this row now exists at the active
            // panel version, which is the precondition an anchor write has. A
            // re-measure sweep that left the row ungrounded would have to be
            // followed by a second full sweep to ground it, against a corpus
            // that moves between passes — so the outcome is written here, in the
            // same pass, against the row that was just measured.
            if source_cf == cf::CF_AGENT_TRANSCRIPTS {
                let record: AgentTranscriptRecord =
                    serde_json::from_slice(&raw).map_err(|error| StorageError::ReadFailed {
                        cf_name: source_cf.to_owned(),
                        detail: format!(
                            "decode authoritative agent transcript row for outcome anchoring key_hex={}: {error}",
                            constellations::hex_encode(&key)
                        ),
                    })?;
                if matches!(
                    constellations::agent_transcript_tool_outcome(&record),
                    constellations::AgentTranscriptToolOutcome::Unadjudicable(_)
                ) {
                    outcome_unadjudicable_rows = outcome_unadjudicable_rows.saturating_add(1);
                }
                match self.put_agent_transcript_outcome_anchor_row(&key, &raw, &record)? {
                    Some(()) => outcome_anchored_rows = outcome_anchored_rows.saturating_add(1),
                    None => outcome_absent_rows = outcome_absent_rows.saturating_add(1),
                }
            }
            if source_cf == cf::CF_ACTION_LOG {
                let record: Value = serde_json::from_slice(&raw).map_err(|error| {
                    StorageError::ReadFailed {
                        cf_name: source_cf.to_owned(),
                        detail: format!(
                            "decode authoritative action audit row for outcome anchoring key_hex={}: {error}",
                            constellations::hex_encode(&key)
                        ),
                    }
                })?;
                match constellations::action_outcome_anchor(&key, &record)? {
                    Some(_) => outcome_anchored_rows = outcome_anchored_rows.saturating_add(1),
                    None => outcome_absent_rows = outcome_absent_rows.saturating_add(1),
                }
            }
            if carry_superseded_anchors {
                carry_targets.push((key, disposition.2));
            }
        }

        // Carry only after every active generation row and fresh outcome in the
        // page is durable. The lineage index makes rows without historical
        // evidence a zero-I/O case; only the small grounded subset needs an
        // authoritative Anchors-CF range read (#1981).
        for (key, active_cx_id) in carry_targets {
            anchor_carry_source_generations_read =
                anchor_carry_source_generations_read.saturating_add(superseded_generation_count);
            let source_key_hex = constellations::hex_encode(&key);
            let prior_anchors = anchor_lineage
                .get(&source_key_hex)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if prior_anchors.is_empty() {
                continue;
            }
            let carried = self.with_vault(
                "calyx_anchor_carry_forward",
                "carry grounded anchors from indexed superseded Base rows",
                true,
                |vault| {
                    carry_forward_grounded_anchors(
                        vault,
                        source_cf,
                        &key,
                        active_cx_id,
                        prior_anchors,
                    )
                },
            )?;
            anchors_carried_forward =
                anchors_carried_forward.saturating_add(carried.anchors_written);
            if carried.anchors_written > 0 {
                rows_anchor_carried = rows_anchor_carried.saturating_add(1);
            }
        }
        let latest_seq = self.with_vault(
            "calyx_temporal_metadata_backfill",
            "read Calyx temporal metadata backfill sequence",
            false,
            |vault| Ok(vault.latest_seq()),
        )?;
        Ok(constellations::TemporalMetadataBackfillReport {
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
        })
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
                episode_constellation_reports(
                    batch.pending_reports,
                    readbacks,
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
                let write = vault
                    .put_grounding_anchors_for_many(
                        calyx_entries,
                        payload,
                        "synapse-outcome-anchors",
                    )
                    .map_err(|source| {
                        calyx_write_failed(
                            "calyx_anchors",
                            "put multi-constellation ledger-stamped grounded anchors",
                            &source,
                        )
                    })?;
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
            &CalyxPressureMaintenance::new(Arc::clone(&self.vault)),
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
            &CalyxPressureMaintenance::new(Arc::clone(&self.vault)),
        )
    }

    fn spawn_pressure_task(&self, storage_path: &Path) -> StorageResult<pressure::PressureTask> {
        pressure::spawn(
            Arc::clone(&self.pressure),
            storage_path.to_path_buf(),
            pressure::PressureConfig::default(),
            Arc::new(CalyxPressureMaintenance::new(Arc::clone(&self.vault))),
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
}

impl CalyxVaultKvRead for SynapseCalyxVault {
    fn vault_id_string(&self) -> String {
        self.vault_id()
    }

    fn latest_seq_value(&self) -> u64 {
        self.latest_seq()
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
}

impl CalyxVaultKvRead for SynapseCalyxReadOnlyVault {
    fn vault_id_string(&self) -> String {
        self.vault_id()
    }

    fn latest_seq_value(&self) -> u64 {
        self.latest_seq()
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
    // Paged rather than materialised (#2041). The whole-vault census is the
    // single largest hold in `storage inspect`: one `scan_kv_range_latest` over
    // every collection, 1,025,928 live rows on the production vault, folded
    // under one acquisition of the MVCC row-table read guard that every vault
    // write must take exclusively. Nothing about the fold needs a single atomic
    // view — it is a sum — so the guard is released every page and the window
    // the census actually observed is reported instead of assumed.
    let sweep = sweep_kv_range_pages(
        vault,
        "<calyx-vault>",
        "calyx_vault_inspect",
        &prefix_range(&[CALYX_KV_DISC]),
        |full_key, stored_value| census.add_row(full_key, stored_value, inspected_at_unix_ms),
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
    if !sweep.atomic() {
        // Never presented as an instant when it is not one. The unpaged census
        // was one atomic view by construction; this one is only atomic when no
        // commit landed mid-sweep, and the caller is told which it got rather
        // than left to assume (#2041).
        tracing::warn!(
            code = "STORAGE_CALYX_INSPECT_CENSUS_INTERVAL",
            site = "calyx_vault_inspect",
            pages = sweep.pages,
            rows_visited = sweep.rows_visited,
            snapshot_seq_first = sweep.snapshot_seq_first,
            snapshot_seq_last = sweep.snapshot_seq_last,
            "the whole-vault census observed an interval rather than one instant; a commit landed between its first and last page"
        );
    }
    Ok(CalyxVaultInspect {
        schema_version,
        vault_id: vault.vault_id_string(),
        latest_seq: vault.latest_seq_value(),
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

struct VerifiedMcpUsageSourceReadback {
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
    let source_readback =
        verify_mcp_usage_source_readback(vault, &expected_source_rows, source_key, raw_bytes)?;
    let source_readback_us = constellations::duration_us(source_readback_started.elapsed());
    let anchor_readback_started = Instant::now();
    let readback_anchor_count = verify_mcp_usage_anchor_readback(vault, context.cx_id, &anchor)?;
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

fn verify_mcp_usage_source_readback(
    vault: &SynapseCalyxVault,
    expected_source_rows: &[SynapseCalyxCfWrite],
    source_key: &[u8],
    raw_bytes: &[u8],
) -> StorageResult<VerifiedMcpUsageSourceReadback> {
    let collection_id = calyx_collection_id_for_cf_write(cf::CF_KV)?;
    let requested_physical_key = encode_calyx_key_for_write(cf::CF_KV, collection_id, source_key)?;
    let mut requested_readback = None;
    let reads = expected_source_rows
        .iter()
        .map(|expected| CfRead::new(ColumnFamily::Kv, expected.key.clone()))
        .collect::<Vec<_>>();
    let actual_rows = vault.read_cf_batch_latest(&reads).map_err(|source| {
        calyx_read_failed(
            cf::CF_KV,
            "read back atomic MCP usage source row batch from one latest view",
            &source,
        )
    })?;
    if actual_rows.len() != expected_source_rows.len() {
        return Err(calyx_write_failed_detail(
            "calyx_mcp_usage_publication",
            format!(
                "atomic MCP usage physical source readback returned {} rows for {} requested keys",
                actual_rows.len(),
                expected_source_rows.len()
            ),
        ));
    }
    for (expected, actual) in expected_source_rows.iter().zip(actual_rows) {
        let Some(actual) = actual else {
            return Err(calyx_write_failed_detail(
                "calyx_mcp_usage_publication",
                format!(
                    "atomic MCP usage physical source readback missing: key_hex={}",
                    constellations::hex_encode(&expected.key)
                ),
            ));
        };
        if actual != expected.value {
            return Err(calyx_write_failed_detail(
                "calyx_mcp_usage_publication",
                format!(
                    "atomic MCP usage physical source readback mismatch: key_hex={} expected_sha256={} actual_sha256={}",
                    constellations::hex_encode(&expected.key),
                    constellations::sha256_hex(&expected.value),
                    constellations::sha256_hex(&actual)
                ),
            ));
        }
        if expected.key == requested_physical_key {
            let envelope = decode_calyx_value_raw(&actual).map_err(|detail| {
                calyx_write_failed_detail(
                    "calyx_mcp_usage_publication",
                    format!(
                        "atomic MCP usage physical source readback has an invalid retention envelope: key_hex={} detail={detail}",
                        constellations::hex_encode(source_key)
                    ),
                )
            })?;
            if envelope.payload != raw_bytes {
                return Err(calyx_write_failed_detail(
                    "calyx_mcp_usage_publication",
                    format!(
                        "atomic MCP usage logical payload readback mismatch: key_hex={} expected_sha256={} actual_sha256={}",
                        constellations::hex_encode(source_key),
                        constellations::sha256_hex(raw_bytes),
                        constellations::sha256_hex(envelope.payload)
                    ),
                ));
            }
            requested_readback = Some(VerifiedMcpUsageSourceReadback {
                exact_match_count: expected_source_rows.len(),
                logical_key_hex: constellations::hex_encode(source_key),
                logical_value_len_bytes: u64::try_from(envelope.payload.len()).unwrap_or(u64::MAX),
                logical_value_sha256: sha256_hex(envelope.payload),
            });
        }
    }
    requested_readback.ok_or_else(|| {
        calyx_write_failed_detail(
            "calyx_mcp_usage_publication",
            format!(
                "atomic MCP usage physical source readback omitted requested key: key_hex={}",
                constellations::hex_encode(source_key)
            ),
        )
    })
}

fn verify_mcp_usage_anchor_readback(
    vault: &SynapseCalyxVault,
    cx_id: CxId,
    expected_anchor: &Anchor,
) -> StorageResult<usize> {
    let anchor_row = vault
        .read_anchor_exact(cx_id, &expected_anchor.kind)
        .map_err(|source| {
            calyx_read_failed(
                "calyx_anchors",
                "read back exact atomic MCP usage grounded anchor",
                &source,
            )
        })?
        .ok_or_else(|| {
            calyx_write_failed_detail(
                "calyx_mcp_usage_publication",
                format!(
                    "atomic MCP usage exact physical anchor row is missing for cx_id={cx_id} kind={}",
                    anchor_kind_label(&expected_anchor.kind)
                ),
            )
        })?;
    if anchor_row.anchor != *expected_anchor {
        return Err(calyx_write_failed_detail(
            "calyx_mcp_usage_publication",
            format!(
                "atomic MCP usage exact physical anchor readback mismatch for cx_id={cx_id} kind={}",
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
    source_readback: &VerifiedMcpUsageSourceReadback,
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
    let occurrence = if let Some(trigger_cx_id) = region_trigger_cx_id {
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
    } else {
        vault.append_recurrence_occurrence_once(
            cx_id,
            event_time_secs,
            observed_at_secs,
            context.to_vec(),
            occurrence_identity_sha256,
        )
    }
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

/// Provenance of one bounded-hold sweep over an ordered Calyx KV range (#2041).
///
/// A sweep trades one long atomic view for many short ones, so the window it
/// observed is a property of the result and is reported rather than assumed.
/// `snapshot_seq_first == snapshot_seq_last` ([`Self::atomic`]) means no commit
/// landed between the first and last page, and only then is the fold's output
/// an exact census of one instant; otherwise it is a census over an interval.
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
/// Paging does not make the work smaller; it makes the *hold* bounded by a
/// constant instead of by the size of the range. The cost is ~27% more total
/// CPU re-merging each page's candidates, and a window that moves: the fold now
/// describes an interval unless [`CalyxKvSweep::atomic`] holds, which is why
/// that flag is returned rather than swallowed.
///
/// # Errors
///
/// Fails closed when a page reports more candidates without a resume cursor, or
/// returns a cursor that does not advance — both would re-read the same page
/// forever — and propagates the visitor's own error verbatim.
fn sweep_kv_range_pages<V>(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    site: &'static str,
    range: &KeyRange,
    mut visit: V,
) -> StorageResult<CalyxKvSweep>
where
    V: FnMut(&[u8], &[u8]) -> StorageResult<()>,
{
    let started = Instant::now();
    let mut cursor: Option<Vec<u8>> = None;
    let mut sweep = CalyxKvSweep::default();
    loop {
        let page = vault
            .scan_kv_range_page_latest(range, cursor.as_deref(), CALYX_INSPECT_SWEEP_PAGE_ROWS)
            .map_err(|source| {
                calyx_read_failed(
                    cf_name,
                    "scan bounded-hold Calyx KV inspection page",
                    &source,
                )
            })?;
        if sweep.pages == 0 {
            sweep.snapshot_seq_first = page.snapshot_seq;
        }
        sweep.snapshot_seq_last = page.snapshot_seq;
        sweep.pages = sweep.pages.saturating_add(1);
        sweep.rows_examined = sweep.rows_examined.saturating_add(page.examined_rows);
        for (key, value) in &page.rows {
            sweep.rows_visited = sweep.rows_visited.saturating_add(1);
            visit(key, value)?;
        }
        if !page.more {
            break;
        }
        let Some(resume) = page.resume_after else {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "CALYX_INSPECT_SWEEP_CURSOR_MISSING: page {} of the {site} sweep reported more candidates but returned no resume cursor, so the sweep cannot advance; remediation=repair the range pager so a page reporting `more` always carries `resume_after`",
                    sweep.pages
                ),
            });
        };
        if cursor
            .as_deref()
            .is_some_and(|previous| resume.as_slice() <= previous)
        {
            return Err(StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: format!(
                    "CALYX_INSPECT_SWEEP_CURSOR_STALLED: page {} of the {site} sweep returned a resume cursor that does not advance past the previous one, so the sweep would re-read the same page forever; remediation=repair the range pager so `resume_after` is strictly greater than the exclusive `after_key` it was given",
                    sweep.pages
                ),
            });
        }
        cursor = Some(resume);
    }
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
        "folded an ordered Calyx KV range page by page, releasing the row-table read guard between pages"
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
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    let mut previous: Option<Vec<u8>> = None;
    sweep_kv_range_pages(vault, cf_name, site, &range, |key, value| {
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
        if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            return Ok(());
        }
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
        visit(&user_key, envelope.payload)?;
        previous = Some(user_key);
        Ok(())
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

fn read_rows_from_vault_range_filtered(
    vault: &impl CalyxVaultKvRead,
    cf_name: &str,
    range: &KeyRange,
    include_expired: bool,
) -> StorageResult<Vec<RawRow>> {
    let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
    let rows = vault
        .scan_kv_range_latest(range)
        .map_err(|source| calyx_read_failed(cf_name, "scan ordered Calyx KV range", &source))?;
    let mut decoded = Vec::with_capacity(rows.len());
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    for (key, value) in rows {
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, &key)?;
        let envelope = decode_calyx_value_raw(&value).map_err(|detail| {
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
        if include_expired || !calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            decoded.push((user_key, envelope.payload.to_vec()));
        }
    }
    if !decoded
        .windows(2)
        .all(|pair| pair[0].0.as_slice() < pair[1].0.as_slice())
    {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: "CALYX_ORDERED_KEY_RANGE_OUT_OF_ORDER: physical ordered namespace did not decode into strictly increasing logical keys; remediation=inspect duplicate/corrupt namespace-one keys before retrying"
                .to_owned(),
        });
    }
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

fn read_ordered_rows_from_vault_page(
    vault: &impl CalyxVaultKvRead,
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
    let page = vault
        .scan_kv_range_page_latest(&range, None, max_rows)
        .map_err(|source| {
            calyx_read_failed(
                cf_name,
                "scan candidate-bounded ordered Calyx logical page",
                &source,
            )
        })?;
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    let mut decoded = Vec::with_capacity(page.rows.len());
    for (physical_key, value) in page.rows {
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, &physical_key)?;
        let envelope = decode_calyx_value_raw(&value).map_err(|detail| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "decode candidate-bounded ordered Calyx logical page value: key_sha256={} detail={detail}",
                sha256_hex(&user_key)
            ),
        })?;
        if !calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            decoded.push((user_key, envelope.payload.to_vec()));
        }
    }
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
    if decoded.is_empty() && page.more {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "CALYX_ORDERED_KEY_PAGE_NO_LOGICAL_PROGRESS: {max_rows} physical candidates produced no live logical row while more candidates remain; remediation=run retention GC or migrate this caller to the opaque ordered-page cursor before retrying"
            ),
        });
    }
    Ok((decoded, page.more))
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
    let rows = vault
        .scan_kv_range_latest(&range)
        .map_err(|source| calyx_read_failed(cf_name, "scan Calyx KV fixed-key range", &source))?;
    let now_ms = vault
        .clock_now_ms()
        .map_err(|source| calyx_read_failed(cf_name, "read Calyx vault clock", &source))?;
    let mut decoded = Vec::with_capacity(rows.len().min(max_rows));
    let mut more = false;
    for (key, value) in rows {
        let user_key = decode_calyx_user_key_for_read(cf_name, collection_id, &key)?;
        let envelope = decode_calyx_value_raw(&value).map_err(|detail| {
            tracing::error!(
                code = error_codes::STORAGE_READ_FAILED,
                cf = cf_name,
                detail,
                "Calyx storage backend rejected malformed KV retention envelope during bounded range scan"
            );
            StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail,
            }
        })?;
        if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            continue;
        }
        if decoded.len() == max_rows {
            more = true;
            break;
        }
        decoded.push((user_key, envelope.payload.to_vec()));
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
) -> StorageResult<EpisodeConstellationBatch> {
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
    Ok(EpisodeConstellationBatch {
        constellations,
        pending_reports,
    })
}

fn episode_constellation_reports(
    pending: Vec<PendingConstellationReport>,
    readbacks: Vec<SynapseCalyxObservationPutReadback>,
    duration_us: u64,
) -> StorageResult<Vec<ConstellationPutReport>> {
    if readbacks.len() != pending.len() {
        return Err(calyx_write_failed_detail(
            "calyx_constellation",
            format!(
                "episode constellation batch returned {} readbacks for {} inputs",
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
                panel_name: SYN_EPISODE_PANEL_NAME,
                panel_version: SYN_EPISODE_PANEL_VERSION,
                source_cf: cf::CF_EPISODES,
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
}

#[derive(Debug)]
struct CalyxRetentionState {
    live_entries: Vec<CalyxRetentionLiveEntry>,
    tombstones: Vec<SynapseCalyxCfWrite>,
    before_live_bytes: u64,
    expired_rows: u64,
    /// Rows GC declined to consider because a live derived constellation still
    /// points at them (#1882). Neither evicted nor counted toward the caps.
    retained_referenced_rows: u64,
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

fn calyx_gc_default_budgets() -> StorageResult<Vec<CalyxGcBudget>> {
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
/// Source column family -> the source row keys a live derived constellation
/// still points at. The GC tick's protection set (#1882).
type DerivedSourceReferences = BTreeMap<String, BTreeSet<Vec<u8>>>;

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

    fn pinned_seq(&self) -> u64 {
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
    mut visit: V,
) -> StorageResult<CalyxPinnedCfWalk>
where
    V: FnMut(&[u8], &[u8]) -> StorageResult<()>,
{
    let started = Instant::now();
    let range = KeyRange::all();
    let mut cursor: Option<Vec<u8>> = None;
    let mut walk = CalyxPinnedCfWalk {
        pinned_seq: reader.pinned_seq(),
        ..CalyxPinnedCfWalk::default()
    };
    loop {
        let page = reader
            .vault
            .scan_cf_range_page_snapshot(
                reader.snapshot(),
                cf,
                &range,
                cursor.as_deref(),
                CALYX_INSPECT_SWEEP_PAGE_ROWS,
            )
            .map_err(|source| {
                calyx_write_failed(
                    reader.cf_name,
                    &format!(
                        "read page {} of the {} {} census at pinned committed sequence {} \
                         ({} row(s) visited over {} ms so far)",
                        walk.pages.saturating_add(1),
                        cf.name(),
                        reader.site,
                        walk.pinned_seq,
                        walk.rows_visited,
                        started.elapsed().as_millis()
                    ),
                    &source,
                )
            })?;
        if page.snapshot_seq != walk.pinned_seq {
            return Err(calyx_write_failed_detail(
                reader.cf_name,
                format!(
                    "CALYX_PINNED_CENSUS_SEQUENCE_DRIFTED: page {} of the {} {} census was served \
                     at committed sequence {} but the census pinned {}; a pinned walk that \
                     silently changes sequence is a census over an interval, which is exactly the \
                     moving window this pin exists to remove; remediation=repair the pinned pager \
                     so every page is served at the sequence its lease pinned",
                    walk.pages.saturating_add(1),
                    cf.name(),
                    reader.site,
                    page.snapshot_seq,
                    walk.pinned_seq
                ),
            ));
        }
        walk.pages = walk.pages.saturating_add(1);
        walk.rows_examined = walk.rows_examined.saturating_add(page.examined_rows);
        for (key, value) in &page.rows {
            walk.rows_visited = walk.rows_visited.saturating_add(1);
            visit(key, value)?;
        }
        if !page.more {
            break;
        }
        // `more` without a cursor, or a cursor that does not advance, would
        // re-read the same page forever. Both are impossible against the
        // documented pager contract, which is exactly why they are worth
        // failing closed on: an unbounded silent loop inside a GC census is a
        // worse outcome than an error.
        let Some(resume) = page.resume_after else {
            return Err(calyx_write_failed_detail(
                reader.cf_name,
                format!(
                    "CALYX_PINNED_CENSUS_CURSOR_MISSING: page {} of the {} {} census reported more \
                     rows but returned no resume cursor, so the census cannot advance; \
                     remediation=repair the range pager so a page reporting `more` always carries \
                     `resume_after`",
                    walk.pages,
                    cf.name(),
                    reader.site
                ),
            ));
        };
        if cursor
            .as_deref()
            .is_some_and(|previous| resume.as_slice() <= previous)
        {
            return Err(calyx_write_failed_detail(
                reader.cf_name,
                format!(
                    "CALYX_PINNED_CENSUS_CURSOR_STALLED: page {} of the {} {} census returned a \
                     resume cursor that does not advance past the previous one, so the census \
                     would re-read the same page forever; remediation=repair the range pager so \
                     `resume_after` is strictly greater than the exclusive `after_key` it was \
                     given",
                    walk.pages,
                    cf.name(),
                    reader.site
                ),
            ));
        }
        cursor = Some(resume);
    }
    tracing::debug!(
        code = "STORAGE_CALYX_PINNED_CENSUS_WALK",
        site = reader.site,
        cf = cf.name(),
        pinned_seq = walk.pinned_seq,
        pages = walk.pages,
        page_rows = CALYX_INSPECT_SWEEP_PAGE_ROWS,
        rows_visited = walk.rows_visited,
        rows_examined = walk.rows_examined,
        elapsed_ms = started.elapsed().as_millis(),
        "folded a column family page by page at one pinned committed sequence, releasing the \
         row-table read guard between pages"
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
fn collect_derived_source_references(
    vault: &SynapseCalyxVault,
) -> StorageResult<(DerivedSourceReferences, gc::DerivedSourceCensus)> {
    let reader = CalyxPinnedReader::pin(
        vault,
        CALYX_GC_CF,
        "derived_source_references",
        CALYX_GC_SOURCE_CENSUS_LEASE_MS,
    )?;
    let mut referenced = DerivedSourceReferences::new();
    let walk = walk_cf_pages_pinned(&reader, ColumnFamily::Base, |_key, value| {
        collect_derived_source_reference(value, &mut referenced).map_err(|source| {
            calyx_write_failed(
                CALYX_GC_CF,
                "index one Base row's derived source reference",
                &source,
            )
        })
    })?;
    let mut referenced_rows = 0_u64;
    for keys in referenced.values() {
        referenced_rows = referenced_rows.saturating_add(calyx_len_to_u64(
            CALYX_GC_CF,
            "Calyx GC protected source rows",
            keys.len(),
        )?);
    }
    let census = gc::DerivedSourceCensus {
        pinned_seq: walk.pinned_seq,
        pages: calyx_len_to_u64(CALYX_GC_CF, "Calyx GC census pages", walk.pages)?,
        base_rows_visited: calyx_len_to_u64(
            CALYX_GC_CF,
            "Calyx GC census Base rows",
            walk.rows_visited,
        )?,
        referenced_column_families: calyx_len_to_u64(
            CALYX_GC_CF,
            "Calyx GC census source column families",
            referenced.len(),
        )?,
        referenced_rows,
    };
    tracing::info!(
        code = "STORAGE_CALYX_GC_SOURCE_CENSUS_COMPLETED",
        pinned_seq = census.pinned_seq,
        pages = census.pages,
        base_rows_visited = census.base_rows_visited,
        referenced_column_families = census.referenced_column_families,
        referenced_rows = census.referenced_rows,
        "indexed every derived constellation's source reference from one pinned committed sequence"
    );
    Ok((referenced, census))
}

/// Indexes one `Base` row's source reference, if it names one.
fn collect_derived_source_reference(
    value: &[u8],
    referenced: &mut DerivedSourceReferences,
) -> Result<(), synapse_calyx::SynapseCalyxError> {
    let to_error = |detail: String| {
        synapse_calyx::SynapseCalyxError::new(
            "SYNAPSE_CALYX_GC_SOURCE_REFERENCE_UNDECODABLE",
            detail,
            "repair or remove the derived constellation naming an undecodable source key; GC must \
             not treat a corrupt reference as an absent one",
        )
    };
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
            return Ok(());
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
        referenced
            .entry(source_cf.clone())
            .or_default()
            .insert(source_key);
    }
    Ok(())
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
) -> StorageResult<gc::GcReport> {
    let now_ms = calyx_clock_now_for_write(vault, CALYX_GC_CF)?;
    let (referenced, source_census) = collect_derived_source_references(vault)?;
    let mut cf_reports = Vec::with_capacity(budgets.len());
    let mut tombstones = Vec::new();
    for budget in budgets {
        cf_reports.push(run_calyx_gc_budget(
            vault,
            *budget,
            now_ms,
            referenced.get(budget.cf_name),
            &mut tombstones,
        )?);
    }

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
        source_census: Some(source_census),
    })
}

fn run_calyx_gc_budget(
    vault: &SynapseCalyxVault,
    budget: CalyxGcBudget,
    now_ms: u64,
    referenced: Option<&BTreeSet<Vec<u8>>>,
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
    let before_live_rows = calyx_len_to_u64(
        budget.cf_name,
        "Calyx GC live row count",
        state.live_entries.len(),
    )?;
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
    let cap_outcome = apply_calyx_gc_cap_eviction(budget, &mut state, before_value)?;
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
    emit_calyx_gc_report(budget, &state, &cap_outcome, before_value, hard_cap_reached);
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
        before_value,
        after_value: cap_outcome.after_value,
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

fn apply_calyx_gc_cap_eviction(
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
            "Calyx storage GC skipped cap eviction for protected operator-owned column family"
        );
        outcome.eviction_skipped_reason = Some(CALYX_GC_PROTECTED_CF_POLICY_SKIPPED);
        return Ok(outcome);
    }

    state.live_entries.sort_by(|left, right| {
        left.written_at_ms
            .cmp(&right.written_at_ms)
            .then_with(|| left.user_key.cmp(&right.user_key))
    });
    for entry in state.live_entries.drain(..) {
        if outcome.after_value <= budget.soft_cap {
            break;
        }
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
        state.tombstones.push(SynapseCalyxCfWrite::new(
            ColumnFamily::Kv,
            entry.full_key,
            tombstone_value(),
        ));
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
    before_value: u64,
    hard_cap_reached: bool,
) {
    if state.expired_rows > 0
        || cap_outcome.cap_evicted_rows > 0
        || cap_outcome.eviction_skipped_reason.is_some()
        || hard_cap_reached
    {
        tracing::info!(
            code = "STORAGE_CALYX_GC_COMPLETED",
            cf = budget.cf_name,
            unit = budget.unit.as_str(),
            expired_rows = state.expired_rows,
            cap_evicted_rows = cap_outcome.cap_evicted_rows,
            before_value,
            after_value = cap_outcome.after_value,
            soft_cap = budget.soft_cap,
            hard_cap = budget.hard_cap,
            hard_cap_reached,
            eviction_skipped_reason = cap_outcome.eviction_skipped_reason.unwrap_or("none"),
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

fn collect_calyx_retention_state(
    vault: &SynapseCalyxVault,
    cf_name: &str,
    collection_id: u64,
    now_ms: u64,
    protected: bool,
    referenced: Option<&BTreeSet<Vec<u8>>>,
) -> StorageResult<CalyxRetentionState> {
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let rows = vault
        .scan_cf_range_latest(ColumnFamily::Kv, &range)
        .map_err(|source| {
            calyx_write_failed(
                cf_name,
                "scan Calyx KV namespace for retention enforcement",
                &source,
            )
        })?;
    let mut state = CalyxRetentionState {
        live_entries: Vec::new(),
        tombstones: Vec::new(),
        before_live_bytes: 0,
        expired_rows: 0,
        retained_referenced_rows: 0,
    };
    for (full_key, value) in rows {
        let user_key = decode_calyx_user_key(collection_id, &full_key).map_err(|detail| {
            calyx_write_failed_detail(
                cf_name,
                format!("decode Calyx retention scan key: {detail}"),
            )
        })?;
        let envelope = decode_calyx_value_raw(&value).map_err(|detail| {
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
        // #1882: a source row a live derived constellation still points at is
        // not garbage, however old it is. Deleting it destroys the only copy of
        // the input bytes that row's CxId addresses, so re-derive, lazy
        // backfill and derivation audit all become impossible with no error at
        // the moment of loss. Retained rows are excluded from cap eviction too,
        // and counted so the retention is reported rather than silent.
        let referenced_by_derived = referenced.is_some_and(|keys| keys.contains(&user_key));
        if referenced_by_derived {
            state.retained_referenced_rows = state.retained_referenced_rows.saturating_add(1);
            continue;
        }
        if calyx_value_is_expired(envelope.expires_at_ms, now_ms) {
            if !protected {
                state.tombstones.push(SynapseCalyxCfWrite::new(
                    ColumnFamily::Kv,
                    full_key,
                    tombstone_value(),
                ));
                state.expired_rows = state.expired_rows.saturating_add(1);
            }
            continue;
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
        state.live_entries.push(CalyxRetentionLiveEntry {
            full_key,
            user_key,
            live_bytes,
            written_at_ms: envelope.written_at_ms,
        });
    }
    Ok(state)
}

fn calyx_retention_default_for_write(cf_name: &str) -> StorageResult<RetentionDefault> {
    DEFAULTS
        .iter()
        .copied()
        .find(|default| default.cf == cf_name)
        .ok_or_else(|| {
            calyx_write_failed_detail(
                cf_name,
                format!("missing RetentionDefault mapping for Calyx column family {cf_name}"),
            )
        })
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
    matches!(cf_name, cf::CF_KV | cf::CF_ROUTINE_STATE)
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

/// Carries a source row's already-observed grounded anchors across a panel bump.
///
/// **#1980.** `cx_id = hash(input_bytes, panel_version, vault_salt)` and an
/// anchor is keyed by `(cx_id, kind)`, so a panel-version bump re-keys every
/// record and orphans every anchor written against the previous generation. The
/// re-measured corpus is then born ungrounded — measured on this vault as
/// `syn-episode-v1` going from 171-of-171 grounded at 1_904_002 to 0-of-171 at
/// 1_964_001, off the same 171 source rows.
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
fn carry_forward_grounded_anchors(
    vault: &SynapseCalyxVault,
    source_cf: &str,
    source_key: &[u8],
    active_cx_id: CxId,
    prior_anchors: &[Anchor],
) -> StorageResult<AnchorCarryForward> {
    let superseded = constellations::superseded_panel_versions_for_source_cf(source_cf)?;
    if superseded.is_empty() {
        return Ok(AnchorCarryForward::default());
    }
    let panel = constellations::anchor_panel_for_source_cf(source_cf)?;

    // Kinds the ACTIVE generation already carries are never overwritten. A
    // freshly derived outcome is the better evidence: it was measured from the
    // row as it stands now, where a carried one is a historical observation.
    // The page-level lineage index already selected the newest superseded
    // observation per kind from the physical historical Base rows. It joins on
    // source identity because current bytes cannot reconstruct an old cx_id for
    // a mutable row (#1981/#1982).
    let mut carry: Vec<Anchor> = Vec::new();
    for anchor in prior_anchors {
        if !(anchor.confidence.is_finite() && anchor.confidence > 0.0) {
            continue;
        }
        // Use the exact physical key that the ledger-stamped write will use.
        // A prefix-range scan is unnecessary here and previously let an
        // existing row reach the integrity writer as a duplicate batch.
        if vault
            .read_anchor_exact(active_cx_id, &anchor.kind)
            .map_err(|error| {
                calyx_read_failed(
                    "calyx_anchor_carry_forward",
                    "read the active generation's exact anchor before carrying",
                    &error,
                )
            })?
            .is_some()
        {
            continue;
        }
        carry.push(anchor.clone());
    }
    if carry.is_empty() {
        return Ok(AnchorCarryForward { anchors_written: 0 });
    }

    let carried_count = carry.len() as u64;
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema": "synapse_anchor_carry_forward/v1",
        "issue": 1980,
        "source_cf": source_cf,
        // Source keys can contain provider/session identifiers; their raw,
        // hex, and digest forms are all long tokens intentionally refused by
        // the ledger secret scanner. The ledger subject already binds this
        // entry to active_cx_id, so retain only non-secret source shape here.
        "source_key_len_bytes": source_key.len(),
        "panel_name": panel.panel_name,
        "to_panel_version": panel.panel_version,
        "from_panel_versions": superseded,
        "anchor_count": carried_count,
    }))
    .map_err(|source| StorageError::EncodeJson {
        type_name: "synapse_anchor_carry_forward_ledger_payload",
        source,
    })?;
    vault
        .put_grounding_anchors(
            active_cx_id,
            carry.clone(),
            payload,
            "synapse-anchor-carry-forward",
        )
        .map_err(|error| {
            calyx_write_failed(
                "calyx_anchor_carry_forward",
                "carry grounded anchors forward across a panel-version bump",
                &error,
            )
        })?;

    // Physical readback of the Anchors CF, not the write's return value: the
    // whole defect class here is a path that reports success while the corpus
    // stays ungrounded.
    let readback = vault.scan_anchors_for_cx(active_cx_id).map_err(|error| {
        calyx_read_failed(
            "calyx_anchor_carry_forward",
            "read back the carried anchors",
            &error,
        )
    })?;
    for anchor in &carry {
        if !readback.iter().any(|row| &row.anchor == anchor) {
            return Err(calyx_write_failed_detail(
                "calyx_anchor_carry_forward",
                format!(
                    "carried anchor is absent from the Anchors CF after the write: \
                     cx_id={active_cx_id} kind={} source_cf={source_cf} source_key_hex={}; \
                     the constellation would be reported re-measured while staying ungrounded",
                    synapse_calyx::anchor_kind_label(&anchor.kind),
                    constellations::hex_encode(source_key),
                ),
            ));
        }
    }

    tracing::info!(
        code = "CALYX_ANCHOR_CARRIED_FORWARD",
        source_cf,
        source_key_hex = %constellations::hex_encode(source_key),
        panel_name = panel.panel_name,
        to_panel_version = panel.panel_version,
        from_panel_versions = ?superseded,
        cx_id = %active_cx_id,
        anchors_carried = carried_count,
        anchors_present_after = readback.len(),
        "grounded anchors carried across a panel-version bump with physical Anchors CF readback"
    );
    Ok(AnchorCarryForward {
        anchors_written: carried_count,
    })
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
