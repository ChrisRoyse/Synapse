use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, RwLock},
    time::Instant,
};

use calyx_aster::{
    cf::{ColumnFamily, KeyRange, prefix_range},
    mvcc::tombstone_value,
    wal,
};
use calyx_core::{Anchor, AnchorKind, AnchorValue, Constellation, CxId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synapse_calyx::{
    SynapseCalyxAnchorBatchWriteReadback, SynapseCalyxAnchorReadback,
    SynapseCalyxAnchorWriteReadback, SynapseCalyxCfRows, SynapseCalyxCfWrite, SynapseCalyxConfig,
    SynapseCalyxError, SynapseCalyxGroundedObservationReadback, SynapseCalyxObservationPutReadback,
    SynapseCalyxReadOnlyVault, SynapseCalyxVault, SynapseCalyxVaultCloseReadback,
    SynapseCalyxVaultStatus,
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
    ConstellationPutReport, NativeConstellationContext, SYN_ACTION_PANEL_NAME,
    SYN_ACTION_PANEL_VERSION, SYN_AGENT_EVENT_PANEL_NAME, SYN_AGENT_EVENT_PANEL_VERSION,
    SYN_AGENT_TRANSCRIPT_PANEL_NAME, SYN_AGENT_TRANSCRIPT_PANEL_VERSION, SYN_EPISODE_PANEL_NAME,
    SYN_EPISODE_PANEL_VERSION, SYN_MCP_USAGE_PANEL_NAME, SYN_MCP_USAGE_PANEL_VERSION,
    SYN_OBSERVATION_PANEL_NAME, SYN_OBSERVATION_PANEL_VERSION, SYN_OUTCOME_PANEL_NAME,
    SYN_OUTCOME_PANEL_VERSION, SYN_PROCESS_PANEL_NAME, SYN_PROCESS_PANEL_VERSION,
    SYN_REFLEX_PANEL_NAME, SYN_REFLEX_PANEL_VERSION, SYN_TIMELINE_PANEL_NAME,
    SYN_TIMELINE_PANEL_VERSION,
};
use crate::{
    CfEstimateMap, OwnedCfWriteBatch, RawRow, ScanWindow, StorageError, StorageResult, cf,
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
const CALYX_KV_NAMESPACE: u64 = 0;
const CALYX_COLLECTION_ID_BASE: u64 = 0x5359_4e43_4600_0000;
const CALYX_METADATA_COLLECTION_ID: u64 = CALYX_COLLECTION_ID_BASE | 0xffff;
const CALYX_GC_CF: &str = "storage_gc";
const CALYX_GC_WAL_RECYCLE_MAX_SEGMENTS: usize = 8;
const CALYX_GC_WAL_RECYCLE_FSYNC_BUDGET: usize = 8;
const CALYX_GC_PROTECTED_CF_POLICY_SKIPPED: &str = "protected_cf_policy_skipped";
const CALYX_GC_CACHE_EVICTIONS_TOTAL: &str = "cache_evictions_total";
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

pub trait StorageBackend: Send + Sync {
    fn kind(&self) -> StorageBackendKind;
    fn put_batch(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()>;
    fn put_batch_pressure_bypass(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()>;
    fn put_cf_batches_pressure_bypass(&self, batches: Vec<OwnedCfWriteBatch>) -> StorageResult<()>;
    fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>>;
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
    fn pressure_level(&self) -> pressure::DiskPressureLevel;
    fn pressure_permits_write(&self, cf_name: &str) -> bool;
    fn pressure_transition_codes(&self) -> StorageResult<Vec<&'static str>>;
    fn pressure_probe_readback(&self) -> StorageResult<pressure::PressureProbeReadback>;
    fn cf_sizes(&self) -> StorageResult<BTreeMap<String, u64>>;
    fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap>;
    fn cf_row_counts(&self) -> StorageResult<BTreeMap<String, u64>>;
    fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap>;
    fn calyx_vault_status(&self) -> StorageResult<SynapseCalyxVaultStatus>;
    fn close_calyx_vault(
        &self,
        reason: &'static str,
    ) -> StorageResult<SynapseCalyxVaultCloseReadback>;
    fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>>;
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
    fn scan_cf_range(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow>;
    fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>>;
    fn compact_cf(&self, cf_name: &str) -> StorageResult<()>;
    fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()>;
}

pub struct CalyxBackend {
    path: PathBuf,
    vault: Arc<CalyxVaultRuntime>,
    pressure: Arc<pressure::PressureState>,
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
    pub fn open(path: &Path, schema_version: u32) -> StorageResult<Self> {
        let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
        let vault = SynapseCalyxVault::open_latest_readback(config)
            .map_err(|source| calyx_open_failed(path, &source))?;
        verify_calyx_schema_version(&vault, path, schema_version)?;
        Ok(Self {
            path: path.to_path_buf(),
            vault: Arc::new(CalyxVaultRuntime::new(vault)),
            pressure: Arc::new(pressure::PressureState::default()),
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
                commit_calyx_rows_to_vault(vault, "<multi-cf>", writes)
            },
        )
    }

    fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        self.with_vault(cf_name, "read Calyx KV row", false, |vault| {
            let collection_id = calyx_collection_id_for_cf_read(cf_name)?;
            let key = encode_calyx_key_for_read(cf_name, collection_id, key)?;
            let snapshot = latest_calyx_seq(vault);
            let value = vault
                .read_cf_at(snapshot, ColumnFamily::Kv, &key)
                .map_err(|source| calyx_read_failed(cf_name, "read Calyx CF row", &source))?;
            let now_ms = calyx_clock_now_for_read(vault, cf_name)?;
            value.map_or(Ok(None), |bytes| {
                decode_calyx_value_for_read(cf_name, &bytes, now_ms)
            })
        })
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

    fn cf_sizes(&self) -> StorageResult<BTreeMap<String, u64>> {
        let mut sizes = BTreeMap::new();
        for cf_name in cf::ALL_COLUMN_FAMILIES {
            let mut bytes = 0_u64;
            for (key, value) in self.read_all_rows(cf_name)? {
                bytes = bytes.saturating_add(key.len() as u64);
                bytes = bytes.saturating_add(value.len() as u64);
            }
            sizes.insert(cf_name.to_owned(), bytes);
        }
        emit_storage_cf_bytes(&sizes);
        Ok(sizes)
    }

    fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap> {
        let sizes = self.cf_sizes()?;
        Ok((sizes, Vec::new()))
    }

    fn cf_row_counts(&self) -> StorageResult<BTreeMap<String, u64>> {
        let mut counts = BTreeMap::new();
        for cf_name in cf::ALL_COLUMN_FAMILIES {
            counts.insert(
                cf_name.to_owned(),
                self.read_all_rows(cf_name)?.len() as u64,
            );
        }
        Ok(counts)
    }

    fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap> {
        let counts = self.cf_row_counts()?;
        Ok((counts, Vec::new()))
    }

    fn calyx_vault_status(&self) -> StorageResult<SynapseCalyxVaultStatus> {
        self.vault.status()
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
                let constellation = constellations::build_timeline_constellation(
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
                                "put timeline observation constellation",
                                &source,
                            )
                        })?;
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
                let readback =
                    vault
                        .put_observation_constellation(constellation)
                        .map_err(|source| {
                            calyx_write_failed(
                                "calyx_constellation",
                                "put episode observation constellation",
                                &source,
                            )
                        })?;
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
        let rows = self.read_all_rows(cf_name)?;
        Ok(rows
            .into_iter()
            .filter(|(key, _value)| key.as_slice() >= start_key && key.starts_with(prefix))
            .collect())
    }

    fn scan_cf_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        let mut rows = self
            .read_all_rows(cf_name)?
            .into_iter()
            .filter(|(key, _value)| key.as_slice() >= start_key);
        let mut window = Vec::new();
        let mut more = false;
        for row in &mut rows {
            if window.len() == max_rows {
                more = true;
                break;
            }
            window.push(row);
        }
        Ok((window, more))
    }

    fn scan_cf_range(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        self.with_vault(cf_name, "scan Calyx KV fixed-key range", false, |vault| {
            read_fixed_width_rows_from_vault_range(vault, cf_name, start_key, end_key, max_rows)
        })
    }

    fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>> {
        if max_rows == 0 {
            return Ok(Vec::new());
        }
        let mut rows = self.read_all_rows(cf_name)?;
        if rows.len() > max_rows {
            rows.drain(0..rows.len() - max_rows);
        }
        Ok(rows)
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
    let vault = SynapseCalyxReadOnlyVault::open_existing(config)
        .map_err(|source| calyx_open_failed(path, &source))?;
    let actual = verify_calyx_schema_version_existing(&vault, path, schema_version)?;
    inspect_calyx_vault_with_schema(&vault, path, actual)
}

fn scan_calyx_cf_read_only(
    path: &Path,
    schema_version: u32,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    require_known_cf_for_read(cf_name)?;
    let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
    let vault = SynapseCalyxReadOnlyVault::open_existing(config)
        .map_err(|source| calyx_open_failed(path, &source))?;
    verify_calyx_schema_version_existing(&vault, path, schema_version)?;
    read_all_rows_from_vault(&vault, cf_name)
}

pub fn scan_calyx_cf_read_only_including_expired(
    path: &Path,
    schema_version: u32,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    require_known_cf_for_read(cf_name)?;
    let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
    let vault = SynapseCalyxReadOnlyVault::open_existing(config)
        .map_err(|source| calyx_open_failed(path, &source))?;
    verify_calyx_schema_version_existing(&vault, path, schema_version)?;
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
    fn read_kv_at(&self, snapshot: u64, key: &[u8]) -> Result<Option<Vec<u8>>, SynapseCalyxError>;
    fn scan_kv_range_at(
        &self,
        snapshot: u64,
        range: &calyx_aster::cf::KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError>;
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

    fn read_kv_at(&self, snapshot: u64, key: &[u8]) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.read_cf_at(snapshot, ColumnFamily::Kv, key)
    }

    fn scan_kv_range_at(
        &self,
        snapshot: u64,
        range: &calyx_aster::cf::KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_range_at(snapshot, ColumnFamily::Kv, range)
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

    fn read_kv_at(&self, snapshot: u64, key: &[u8]) -> Result<Option<Vec<u8>>, SynapseCalyxError> {
        self.read_cf_at(snapshot, ColumnFamily::Kv, key)
    }

    fn scan_kv_range_at(
        &self,
        snapshot: u64,
        range: &calyx_aster::cf::KeyRange,
    ) -> Result<SynapseCalyxCfRows, SynapseCalyxError> {
        self.scan_cf_range_at(snapshot, ColumnFamily::Kv, range)
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
    let rows = vault
        .scan_kv_range_at(vault.latest_seq_value(), &prefix_range(&[CALYX_KV_DISC]))
        .map_err(|source| {
            calyx_read_failed(
                "<calyx-vault>",
                "scan physical Calyx KV collections",
                &source,
            )
        })?;
    let mut collections: BTreeMap<String, CalyxVaultCollectionInspect> = BTreeMap::new();
    let mut totals = CalyxVaultTotals::default();
    for (full_key, stored_value) in rows {
        let key = decode_calyx_key_parts(&full_key).map_err(|detail| StorageError::ReadFailed {
            cf_name: "<calyx-vault>".to_owned(),
            detail,
        })?;
        let collection_name = calyx_collection_report_name(key.collection_id, key.namespace);
        let entry = collections
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
            decode_calyx_value_raw(&stored_value).map_err(|detail| StorageError::ReadFailed {
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
        totals.add(expired, user_key_bytes, payload_bytes, stored_value_bytes)?;
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
        collections,
    })
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
    if full_key.len() < 1 + 8 + 8 + 2 {
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
    let len = usize::from(u16::from_be_bytes([full_key[17], full_key[18]]));
    let Some(user_key) = full_key.get(19..19 + len) else {
        return Err("Calyx KV key length prefix exceeds the stored key".to_owned());
    };
    if full_key.len() != 19 + len {
        return Err("Calyx KV key has trailing bytes after the user key".to_owned());
    }
    Ok(CalyxKeyParts {
        collection_id: u64::from_be_bytes(collection_bytes),
        namespace: u64::from_be_bytes(namespace_bytes),
        user_key: user_key.to_vec(),
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
    let snapshot = vault.latest_seq();
    let collection_id = calyx_collection_id_for_cf_write(cf::CF_KV)?;
    let requested_physical_key = encode_calyx_key_for_write(cf::CF_KV, collection_id, source_key)?;
    let mut requested_readback = None;
    for expected in expected_source_rows {
        let actual = vault
            .read_cf_at(snapshot, ColumnFamily::Kv, &expected.key)
            .map_err(|source| {
                calyx_read_failed(cf::CF_KV, "read back atomic MCP usage source row", &source)
            })?;
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
    let key = encode_calyx_key(CALYX_METADATA_COLLECTION_ID, SCHEMA_VERSION_KEY)
        .map_err(|detail| calyx_open_failed_detail(path, detail))?;
    let snapshot = latest_calyx_seq(vault);
    let existing = vault
        .read_cf_at(snapshot, ColumnFamily::Kv, &key)
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
    let key = encode_calyx_key(CALYX_METADATA_COLLECTION_ID, SCHEMA_VERSION_KEY)
        .map_err(|detail| calyx_open_failed_detail(path, detail))?;
    let Some(value) = vault
        .read_kv_at(vault.latest_seq_value(), &key)
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

fn decode_schema_version(bytes: &[u8]) -> Option<u32> {
    <[u8; 4]>::try_from(bytes).ok().map(u32::from_be_bytes)
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
    let snapshot = vault.latest_seq_value();
    let rows = vault
        .scan_kv_range_at(snapshot, &range)
        .map_err(|source| calyx_read_failed(cf_name, "scan Calyx KV namespace", &source))?;
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
    decoded.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(decoded)
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
        .scan_kv_range_at(vault.latest_seq_value(), &range)
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

fn latest_calyx_seq(vault: &SynapseCalyxVault) -> u64 {
    vault.latest_seq()
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
    vault
        .flush()
        .map_err(|source| calyx_write_failed(cf_name, "flush Calyx CF batch", &source))
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

fn run_calyx_gc_budgets(
    vault: &SynapseCalyxVault,
    budgets: &[CalyxGcBudget],
) -> StorageResult<gc::GcReport> {
    let now_ms = calyx_clock_now_for_write(vault, CALYX_GC_CF)?;
    let mut cf_reports = Vec::with_capacity(budgets.len());
    let mut tombstones = Vec::new();
    for budget in budgets {
        cf_reports.push(run_calyx_gc_budget(
            vault,
            *budget,
            now_ms,
            &mut tombstones,
        )?);
    }

    if !tombstones.is_empty() {
        let tombstone_rows =
            calyx_len_to_u64(CALYX_GC_CF, "Calyx GC tombstones", tombstones.len())?;
        commit_calyx_rows_to_vault(vault, CALYX_GC_CF, tombstones)?;
        vault.purge_kv_tombstones().map_err(|source| {
            calyx_write_failed(CALYX_GC_CF, "purge Calyx KV tombstones after GC", &source)
        })?;
        tracing::info!(
            code = "STORAGE_CALYX_GC_TOMBSTONES_PURGED",
            tombstone_rows,
            "Calyx storage GC purged committed KV tombstones"
        );
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

    Ok(gc::GcReport { cf_reports })
}

fn run_calyx_gc_budget(
    vault: &SynapseCalyxVault,
    budget: CalyxGcBudget,
    now_ms: u64,
    pending_tombstones: &mut Vec<SynapseCalyxCfWrite>,
) -> StorageResult<gc::GcCfReport> {
    let collection_id = calyx_collection_id_for_cf_write(budget.cf_name)?;
    let mut state = collect_calyx_retention_state(
        vault,
        budget.cf_name,
        collection_id,
        now_ms,
        budget.protected,
    )?;
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
) -> StorageResult<CalyxRetentionState> {
    let range = prefix_range(&calyx_namespace_prefix(collection_id));
    let rows = vault
        .scan_cf_range_at(latest_calyx_seq(vault), ColumnFamily::Kv, &range)
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
    let user_key_len = u16::try_from(user_key.len()).map_err(|_error| {
        format!(
            "Calyx Synapse KV envelope supports keys up to {} bytes; got {}",
            u16::MAX,
            user_key.len()
        )
    })?;
    let mut key = calyx_namespace_prefix(collection_id);
    key.extend_from_slice(&user_key_len.to_be_bytes());
    key.extend_from_slice(user_key);
    Ok(key)
}

fn calyx_namespace_prefix(collection_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 8 + 8);
    key.push(CALYX_KV_DISC);
    key.extend_from_slice(&collection_id.to_be_bytes());
    key.extend_from_slice(&CALYX_KV_NAMESPACE.to_be_bytes());
    key
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
        return Err("Calyx KV scan returned a key outside the requested namespace".to_owned());
    };
    let Some(len_bytes) = rest.get(0..2) else {
        return Err("Calyx KV key is missing its user-key length prefix".to_owned());
    };
    let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    let Some(user_key) = rest.get(2..2 + len) else {
        return Err("Calyx KV key length prefix exceeds the stored key".to_owned());
    };
    if rest.len() != 2 + len {
        return Err("Calyx KV key has trailing bytes after the user key".to_owned());
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
