use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use calyx_aster::{
    cf::{ColumnFamily, prefix_range},
    mvcc::tombstone_value,
    wal,
};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, ColumnFamilyRef, DB, DBCompressionType,
    Direction, IteratorMode, Options, SliceTransform, WriteBatch,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synapse_calyx::{
    SynapseCalyxCfRows, SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxError,
    SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};
use synapse_core::{
    error_codes,
    retention::{DEFAULTS, RetentionDefault, RetentionTtl},
};

use crate::{
    CfEstimateMap, OwnedCfWriteBatch, RawRow, ScanWindow, StorageError, StorageResult, batch, cf,
    compaction, gc, pressure,
};

const MIB: usize = 1024 * 1024;
const DEFAULT_WRITE_BUFFER_BYTES: usize = 64 * MIB;
const MODEL_CACHE_WRITE_BUFFER_BYTES: usize = 256 * MIB;
const BLOCK_CACHE_BYTES: usize = 64 * MIB;
const SCHEMA_VERSION_KEY: &[u8] = b"__schema_version";
const TIMELINE_PERIODIC_COMPACTION_SECONDS: u64 = 86_400;
const STORAGE_WRITES_SHED_TOTAL: &str = "storage_writes_shed_total";
const STORAGE_CF_BYTES: &str = "storage_cf_bytes";
const ESTIMATE_LIVE_DATA_SIZE: &str = "rocksdb.estimate-live-data-size";
const ESTIMATE_NUM_KEYS: &str = "rocksdb.estimate-num-keys";
const CALYX_KV_DISC: u8 = 0x03;
const CALYX_KV_VALUE_VERSION_V1: u8 = 0x01;
const CALYX_KV_VALUE_VERSION: u8 = 0x02;
const CALYX_KV_VALUE_V1_HEADER_BYTES: usize = 1 + 8;
const CALYX_KV_VALUE_HEADER_BYTES: usize = 1 + 8 + 8;
const CALYX_KV_NAMESPACE: u64 = 0;
const CALYX_COLLECTION_ID_BASE: u64 = 0x5359_4e43_4600_0000;
const CALYX_METADATA_COLLECTION_ID: u64 = CALYX_COLLECTION_ID_BASE | 0xffff;
const CALYX_UNSUPPORTED_MAINTENANCE_DETAIL: &str = "storage_backend=\"calyx\" supports Db read/write/scan/delete/pressure/GC parity; direct compact_cf and compact_cf_range are unavailable because Synapse column families are logical namespaces inside the physical Calyx KV column family";
const CALYX_GC_CF: &str = "storage_gc";
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackendKind {
    #[default]
    RocksDb,
    Calyx,
}

impl StorageBackendKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RocksDb => "rocksdb",
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
            "" | "rocksdb" => Ok(Self::RocksDb),
            "calyx" => Ok(Self::Calyx),
            other => Err(format!(
                "storage_backend must be \"rocksdb\" or \"calyx\"; got {other:?}"
            )),
        }
    }
}

pub trait StorageBackend: Send + Sync {
    fn kind(&self) -> StorageBackendKind;
    fn put_batch(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()>;
    fn put_batch_pressure_bypass(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()>;
    fn put_calyx_migration_batch_pressure_bypass(
        &self,
        cf_name: &str,
        rows: Vec<CalyxMigrationRow>,
    ) -> StorageResult<()>;
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
    fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>>;
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
    fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>>;
    fn compact_cf(&self, cf_name: &str) -> StorageResult<()>;
    fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalyxMigrationRow {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub written_at_ms: u64,
    pub expires_at_ms: u64,
}

pub struct RocksDbBackend {
    batcher: batch::Batcher,
    inner: Arc<DB>,
    pressure: Arc<pressure::PressureState>,
}

impl RocksDbBackend {
    pub fn open(path: &Path, schema_version: u32) -> StorageResult<Self> {
        let options = db_options();
        // An error here means the database does not exist yet (fresh open);
        // create_missing_column_families covers that path.
        let existing_cfs = DB::list_cf(&Options::default(), path).unwrap_or_default();
        let unknown_cfs: Vec<String> = existing_cfs
            .into_iter()
            .filter(|name| {
                name != rocksdb::DEFAULT_COLUMN_FAMILY_NAME
                    && !cf::ALL_COLUMN_FAMILIES.contains(&name.as_str())
            })
            .collect();
        for name in &unknown_cfs {
            tracing::warn!(
                code = "STORAGE_UNKNOWN_CF_OPENED",
                cf_name = %name,
                backend = StorageBackendKind::RocksDb.as_str(),
                "database holds a column family this binary does not know; \
                 opening it untouched with default options (newer-binary data, \
                 preserved for rollback safety)"
            );
        }
        let descriptors = cf::ALL_COLUMN_FAMILIES
            .into_iter()
            .map(|name| ColumnFamilyDescriptor::new(name, cf_options(name)))
            .chain(
                unknown_cfs
                    .iter()
                    .map(|name| ColumnFamilyDescriptor::new(name, Options::default())),
            );
        let inner = DB::open_cf_descriptors(&options, path, descriptors)
            .map_err(|source| open_failed(path, &source))?;
        verify_schema_version(&inner, path, schema_version)?;

        for name in cf::ALL_COLUMN_FAMILIES {
            if inner.cf_handle(name).is_none() {
                return Err(open_failed_detail(
                    path,
                    format!("column family handle missing after open: {name}"),
                ));
            }
        }

        let inner = Arc::new(inner);
        let batcher = batch::Batcher::spawn(Arc::clone(&inner));
        let pressure = Arc::new(pressure::PressureState::default());

        Ok(Self {
            batcher,
            inner,
            pressure,
        })
    }

    fn cf_handle(&self, cf_name: &str) -> StorageResult<ColumnFamilyRef<'_>> {
        self.inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })
    }

    fn cf_property_estimates(&self, property: &str) -> StorageResult<CfEstimateMap> {
        let mut values = BTreeMap::new();
        let mut missing = Vec::new();
        for cf_name in cf::ALL_COLUMN_FAMILIES {
            let handle = self.cf_handle(cf_name)?;
            if let Some(value) = self
                .inner
                .property_int_value_cf(&handle, property)
                .map_err(|source| StorageError::ReadFailed {
                    cf_name: cf_name.to_owned(),
                    detail: source.to_string(),
                })?
            {
                values.insert(cf_name.to_owned(), value);
            } else {
                values.insert(cf_name.to_owned(), 0);
                missing.push(cf_name.to_owned());
            }
        }
        Ok((values, missing))
    }
}

impl StorageBackend for RocksDbBackend {
    fn kind(&self) -> StorageBackendKind {
        StorageBackendKind::RocksDb
    }

    fn put_batch(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()> {
        self.inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
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
        self.batcher.put_batch(cf_name, rows)
    }

    fn put_batch_pressure_bypass(&self, cf_name: &str, rows: Vec<RawRow>) -> StorageResult<()> {
        let cf = self
            .inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        for (key, value) in rows {
            batch.put_cf(&cf, key, value);
        }
        self.inner
            .write(batch)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
        self.inner
            .flush_cf(&cf)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })
    }

    fn put_calyx_migration_batch_pressure_bypass(
        &self,
        _cf_name: &str,
        _rows: Vec<CalyxMigrationRow>,
    ) -> StorageResult<()> {
        Err(StorageError::BackendUnavailable {
            backend: self.kind().as_str().to_owned(),
            detail: "Calyx migration envelope writes require storage_backend=\"calyx\"".to_owned(),
        })
    }

    fn put_cf_batches_pressure_bypass(&self, batches: Vec<OwnedCfWriteBatch>) -> StorageResult<()> {
        if batches.iter().all(|(_cf_name, rows)| rows.is_empty()) {
            return Ok(());
        }
        let mut handles = Vec::with_capacity(batches.len());
        for (cf_name, _rows) in &batches {
            let handle =
                self.inner
                    .cf_handle(cf_name)
                    .ok_or_else(|| StorageError::WriteFailed {
                        cf_name: cf_name.to_owned(),
                        detail: "column family handle missing".to_owned(),
                    })?;
            handles.push(handle);
        }
        let mut batch = WriteBatch::default();
        for ((_cf_name, rows), handle) in batches.iter().zip(&handles) {
            for (key, value) in rows {
                batch.put_cf(handle, key, value);
            }
        }
        self.inner
            .write(batch)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: "<multi-cf>".to_owned(),
                detail: source.to_string(),
            })?;
        for ((cf_name, _rows), handle) in batches.into_iter().zip(handles) {
            self.inner
                .flush_cf(&handle)
                .map_err(|source| StorageError::WriteFailed {
                    cf_name,
                    detail: source.to_string(),
                })?;
        }
        Ok(())
    }

    fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        let handle = self.cf_handle(cf_name)?;
        self.inner
            .get_cf(&handle, key)
            .map_err(|source| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })
    }

    fn mutate_batch_pressure_bypass(
        &self,
        cf_name: &str,
        deletes: Vec<Vec<u8>>,
        puts: Vec<RawRow>,
    ) -> StorageResult<()> {
        let cf = self
            .inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        if deletes.is_empty() && puts.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        for key in deletes {
            batch.delete_cf(&cf, key);
        }
        for (key, value) in puts {
            batch.put_cf(&cf, key, value);
        }
        self.inner
            .write(batch)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
        self.inner
            .flush_cf(&cf)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })
    }

    fn delete_batch(&self, cf_name: &str, keys: Vec<Vec<u8>>) -> StorageResult<()> {
        let cf = self
            .inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        if keys.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        for key in keys {
            batch.delete_cf(&cf, key);
        }
        self.inner
            .write(batch)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
        self.inner
            .flush_cf(&cf)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })
    }

    fn flush(&self) -> StorageResult<()> {
        self.batcher.flush()
    }

    fn run_gc_once(&self) -> StorageResult<gc::GcReport> {
        gc::run_once(&self.inner, &gc::GcConfig::from_retention_defaults())
    }

    fn run_gc_once_with_row_caps(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<gc::GcReport> {
        gc::run_once(
            &self.inner,
            &gc::GcConfig::for_row_caps(
                Duration::from_mins(5),
                cf_name,
                soft_cap_rows,
                hard_cap_rows,
            ),
        )
    }

    fn spawn_gc_task(&self) -> StorageResult<gc::GcTask> {
        gc::spawn(
            Arc::clone(&self.inner),
            gc::GcConfig::from_retention_defaults(),
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
            for (key, value) in self.scan_cf(cf_name)? {
                bytes = bytes.saturating_add(key.len() as u64);
                bytes = bytes.saturating_add(value.len() as u64);
            }
            sizes.insert(cf_name.to_owned(), bytes);
        }
        emit_storage_cf_bytes(&sizes);
        Ok(sizes)
    }

    fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap> {
        let estimates = self.cf_property_estimates(ESTIMATE_LIVE_DATA_SIZE)?;
        emit_storage_cf_bytes(&estimates.0);
        Ok(estimates)
    }

    fn cf_row_counts(&self) -> StorageResult<BTreeMap<String, u64>> {
        let mut counts = BTreeMap::new();
        for cf_name in cf::ALL_COLUMN_FAMILIES {
            counts.insert(cf_name.to_owned(), self.scan_cf(cf_name)?.len() as u64);
        }
        Ok(counts)
    }

    fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap> {
        self.cf_property_estimates(ESTIMATE_NUM_KEYS)
    }

    fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>> {
        Ok(None)
    }

    fn run_pressure_check_once(
        &self,
        storage_path: &Path,
    ) -> StorageResult<pressure::PressureReport> {
        pressure::run_once(
            &self.pressure,
            storage_path,
            &pressure::PressureConfig::default(),
            &pressure::RocksDbPressureMaintenance::new(Arc::clone(&self.inner)),
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
            &pressure::RocksDbPressureMaintenance::new(Arc::clone(&self.inner)),
        )
    }

    fn spawn_pressure_task(&self, storage_path: &Path) -> StorageResult<pressure::PressureTask> {
        pressure::spawn(
            Arc::clone(&self.pressure),
            storage_path.to_path_buf(),
            pressure::PressureConfig::default(),
            Arc::new(pressure::RocksDbPressureMaintenance::new(Arc::clone(
                &self.inner,
            ))),
        )
    }

    fn scan_cf(&self, cf_name: &str) -> StorageResult<Vec<RawRow>> {
        let handle = self.cf_handle(cf_name)?;
        let mut rows = Vec::new();
        for item in self.inner.iterator_cf(&handle, IteratorMode::Start) {
            let (key, value) = item.map_err(|source| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
            rows.push((key.to_vec(), value.to_vec()));
        }
        Ok(rows)
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
        let handle = self.cf_handle(cf_name)?;
        let mut rows = Vec::new();
        for item in self
            .inner
            .iterator_cf(&handle, IteratorMode::From(start_key, Direction::Forward))
        {
            let (key, value) = item.map_err(|source| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
            if !key.starts_with(prefix) {
                break;
            }
            rows.push((key.to_vec(), value.to_vec()));
        }
        Ok(rows)
    }

    fn scan_cf_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        let handle = self.cf_handle(cf_name)?;
        let mut rows = Vec::new();
        let mut more = false;
        for item in self
            .inner
            .iterator_cf(&handle, IteratorMode::From(start_key, Direction::Forward))
        {
            let (key, value) = item.map_err(|source| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
            if rows.len() == max_rows {
                more = true;
                break;
            }
            rows.push((key.to_vec(), value.to_vec()));
        }
        Ok((rows, more))
    }

    fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>> {
        if max_rows == 0 {
            return Ok(Vec::new());
        }
        let handle = self.cf_handle(cf_name)?;
        let mut rows = Vec::new();
        for item in self.inner.iterator_cf(&handle, IteratorMode::End) {
            let (key, value) = item.map_err(|source| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })?;
            rows.push((key.to_vec(), value.to_vec()));
            if rows.len() >= max_rows {
                break;
            }
        }
        rows.reverse();
        Ok(rows)
    }

    fn compact_cf(&self, cf_name: &str) -> StorageResult<()> {
        let handle = self.cf_handle(cf_name)?;
        self.inner
            .compact_range_cf(&handle, None::<&[u8]>, None::<&[u8]>);
        Ok(())
    }

    fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()> {
        let handle = self.cf_handle(cf_name)?;
        self.inner.compact_range_cf(&handle, Some(start), Some(end));
        Ok(())
    }
}

pub struct CalyxBackend {
    path: PathBuf,
    vault: Arc<Mutex<Option<SynapseCalyxVault>>>,
    pressure: Arc<pressure::PressureState>,
}

impl CalyxBackend {
    pub fn open(path: &Path, schema_version: u32) -> StorageResult<Self> {
        let config = SynapseCalyxConfig::from_vault_dir(path.to_path_buf());
        let vault =
            SynapseCalyxVault::open(config).map_err(|source| calyx_open_failed(path, &source))?;
        verify_calyx_schema_version(&vault, path, schema_version)?;
        Ok(Self {
            path: path.to_path_buf(),
            vault: Arc::new(Mutex::new(Some(vault))),
            pressure: Arc::new(pressure::PressureState::default()),
        })
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the mutex guard intentionally owns the single process-local Calyx vault for the whole storage operation"
    )]
    fn with_vault<T>(
        &self,
        cf_name: &str,
        operation: &'static str,
        write: bool,
        f: impl FnOnce(&SynapseCalyxVault) -> StorageResult<T>,
    ) -> StorageResult<T> {
        let guard = self.vault.lock().map_err(|poisoned| {
            calyx_operation_failed(
                cf_name,
                write,
                format!("{operation}: Calyx vault mutex poisoned: {poisoned}"),
            )
        })?;
        let Some(vault) = guard.as_ref() else {
            return Err(calyx_operation_failed(
                cf_name,
                write,
                format!("{operation}: Calyx vault handle has already been closed"),
            ));
        };
        f(vault)
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

impl Drop for CalyxBackend {
    fn drop(&mut self) {
        let vault = match self.vault.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => {
                tracing::error!(
                    code = "STORAGE_CALYX_DROP_LOCK_POISONED",
                    error = %poisoned,
                    "Calyx storage backend mutex poisoned during drop; attempting close anyway"
                );
                poisoned.into_inner().take()
            }
        };
        if let Some(vault) = vault
            && let Err(error) = vault.close("synapse_storage_calyx_backend_drop")
        {
            tracing::error!(
                code = error.code,
                error = %error,
                "Calyx storage backend close failed during drop"
            );
        }
    }
}

struct CalyxPressureMaintenance {
    vault: Arc<Mutex<Option<SynapseCalyxVault>>>,
}

impl CalyxPressureMaintenance {
    const fn new(vault: Arc<Mutex<Option<SynapseCalyxVault>>>) -> Self {
        Self { vault }
    }
}

impl pressure::PressureMaintenance for CalyxPressureMaintenance {
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the vault mutex intentionally serializes physical Calyx compaction with storage reads/writes"
    )]
    fn compact_for_pressure(&self) -> StorageResult<Vec<&'static str>> {
        let guard = self.vault.lock().map_err(|poisoned| {
            calyx_operation_failed(
                pressure::PRESSURE_CF,
                true,
                format!("compact Calyx KV for pressure: vault mutex poisoned: {poisoned}"),
            )
        })?;
        let Some(vault) = guard.as_ref() else {
            return Err(calyx_operation_failed(
                pressure::PRESSURE_CF,
                true,
                "compact Calyx KV for pressure: vault handle has already been closed".to_owned(),
            ));
        };
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
    }
}

struct CalyxGcRunner {
    vault: Arc<Mutex<Option<SynapseCalyxVault>>>,
}

impl CalyxGcRunner {
    const fn new(vault: Arc<Mutex<Option<SynapseCalyxVault>>>) -> Self {
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

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the vault mutex intentionally serializes physical Calyx GC scans, tombstone writes, and tombstone purges"
    )]
    fn run_with_budgets(&self, budgets: &[CalyxGcBudget]) -> StorageResult<gc::GcReport> {
        let guard = self.vault.lock().map_err(|poisoned| {
            calyx_operation_failed(
                CALYX_GC_CF,
                true,
                format!("run Calyx GC: vault mutex poisoned: {poisoned}"),
            )
        })?;
        let Some(vault) = guard.as_ref() else {
            return Err(calyx_operation_failed(
                CALYX_GC_CF,
                true,
                "run Calyx GC: vault handle has already been closed".to_owned(),
            ));
        };
        run_calyx_gc_budgets(vault, budgets)
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

    fn put_calyx_migration_batch_pressure_bypass(
        &self,
        cf_name: &str,
        rows: Vec<CalyxMigrationRow>,
    ) -> StorageResult<()> {
        let collection_id = calyx_collection_id_for_cf_write(cf_name)?;
        if rows.is_empty() {
            return Ok(());
        }
        self.with_vault(cf_name, "write Calyx migration KV batch", true, |vault| {
            let writes = rows
                .into_iter()
                .map(|row| {
                    encode_calyx_key_for_write(cf_name, collection_id, &row.key).map(|key| {
                        SynapseCalyxCfWrite::new(
                            ColumnFamily::Kv,
                            key,
                            encode_calyx_value(row.expires_at_ms, row.written_at_ms, &row.value),
                        )
                    })
                })
                .collect::<StorageResult<Vec<_>>>()?;
            commit_calyx_rows_to_vault(vault, cf_name, writes)
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

    fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>> {
        self.with_vault(
            "<calyx-vault>",
            "inspect Calyx vault collections",
            false,
            |vault| inspect_calyx_vault(vault, &self.path).map(Some),
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

    fn compact_cf(&self, _cf_name: &str) -> StorageResult<()> {
        Err(calyx_unsupported_maintenance("compact_cf"))
    }

    fn compact_cf_range(&self, _cf_name: &str, _start: &[u8], _end: &[u8]) -> StorageResult<()> {
        Err(calyx_unsupported_maintenance("compact_cf_range"))
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
        StorageBackendKind::RocksDb => scan_rocksdb_cf_read_only(path, schema_version, cf_name),
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

fn scan_rocksdb_cf_read_only(
    path: &Path,
    schema_version: u32,
    cf_name: &str,
) -> StorageResult<Vec<RawRow>> {
    require_known_cf_for_read(cf_name)?;
    let existing_cfs = DB::list_cf(&Options::default(), path)
        .map_err(|source| open_failed_detail(path, format!("not an existing RocksDB: {source}")))?;
    if !existing_cfs.iter().any(|name| name == cf_name) {
        return Err(StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!(
                "column family {cf_name} does not exist in {}; present: {existing_cfs:?}",
                path.display()
            ),
        });
    }
    let db = DB::open_cf_for_read_only(&Options::default(), path, &existing_cfs, false)
        .map_err(|source| open_failed(path, &source))?;
    verify_rocksdb_schema_version_read_only(&db, path, schema_version)?;
    let handle = db
        .cf_handle(cf_name)
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!("column family handle missing after read-only open: {cf_name}"),
        })?;
    let mut rows = Vec::new();
    for item in db.iterator_cf(&handle, IteratorMode::Start) {
        let (key, value) = item.map_err(|source| StorageError::ReadFailed {
            cf_name: cf_name.to_owned(),
            detail: format!("read-only RocksDB row iteration failed: {source}"),
        })?;
        rows.push((key.to_vec(), value.to_vec()));
    }
    Ok(rows)
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

fn verify_rocksdb_schema_version_read_only(
    db: &DB,
    path: &Path,
    schema_version: u32,
) -> StorageResult<()> {
    let Some(value) = db
        .get(SCHEMA_VERSION_KEY)
        .map_err(|source| open_failed(path, &source))?
    else {
        return Err(StorageError::SchemaMismatch {
            expected: schema_version,
            actual: 0,
        });
    };
    let actual = decode_schema_version(&value).unwrap_or_default();
    if actual == schema_version {
        Ok(())
    } else {
        Err(StorageError::SchemaMismatch {
            expected: schema_version,
            actual,
        })
    }
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
    cf_name == cf::CF_ROUTINE_STATE
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
    StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
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
    StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
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

fn calyx_unsupported_maintenance(operation: &'static str) -> StorageError {
    tracing::error!(
        code = error_codes::STORAGE_BACKEND_UNIMPLEMENTED,
        backend = StorageBackendKind::Calyx.as_str(),
        operation,
        detail = CALYX_UNSUPPORTED_MAINTENANCE_DETAIL,
        "Calyx storage backend maintenance API is unavailable"
    );
    StorageError::BackendUnavailable {
        backend: StorageBackendKind::Calyx.as_str().to_owned(),
        detail: format!("{operation}: {CALYX_UNSUPPORTED_MAINTENANCE_DETAIL}"),
    }
}

fn db_options() -> Options {
    let mut options = Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    options.set_max_background_jobs(2);
    options.set_compression_type(DBCompressionType::Lz4);
    options.set_max_open_files(256);
    options.set_keep_log_file_num(8);
    options.set_write_buffer_size(DEFAULT_WRITE_BUFFER_BYTES);
    options.set_max_write_buffer_number(3);
    options.set_target_file_size_base(DEFAULT_WRITE_BUFFER_BYTES as u64);
    options.set_level_zero_file_num_compaction_trigger(4);
    apply_block_cache(&mut options);
    options
}

fn cf_options(name: &'static str) -> Options {
    let mut options = Options::default();
    options.set_write_buffer_size(DEFAULT_WRITE_BUFFER_BYTES);
    options.set_max_write_buffer_number(3);
    options.set_target_file_size_base(DEFAULT_WRITE_BUFFER_BYTES as u64);
    options.set_level_zero_file_num_compaction_trigger(4);
    options.set_compression_type(DBCompressionType::Lz4);

    match name {
        cf::CF_EVENTS | cf::CF_ACTION_LOG | cf::CF_REFLEX_AUDIT => {
            options.set_compression_type(DBCompressionType::Lz4);
            options.set_prefix_extractor(SliceTransform::create_fixed_prefix(8));
        }
        cf::CF_MODEL_CACHE => {
            options.set_compression_type(DBCompressionType::None);
            options.set_write_buffer_size(MODEL_CACHE_WRITE_BUFFER_BYTES);
        }
        cf::CF_OBSERVATIONS | cf::CF_SESSIONS => {
            options.set_compression_type(DBCompressionType::Zstd);
        }
        cf::CF_TIMELINE | cf::CF_EPISODES | cf::CF_AGENT_EVENTS => {
            options.set_compression_type(DBCompressionType::Zstd);
            options.set_prefix_extractor(SliceTransform::create_fixed_prefix(8));
            options.set_periodic_compaction_seconds(TIMELINE_PERIODIC_COMPACTION_SECONDS);
        }
        cf::CF_AGENT_TRANSCRIPTS => {
            options.set_compression_type(DBCompressionType::Zstd);
            options.set_periodic_compaction_seconds(TIMELINE_PERIODIC_COMPACTION_SECONDS);
        }
        _ => {}
    }

    if cf_has_ttl(name) {
        options.set_periodic_compaction_seconds(TIMELINE_PERIODIC_COMPACTION_SECONDS);
    }

    compaction::install_ttl_filter(&mut options, name);
    apply_block_cache(&mut options);
    options
}

fn cf_has_ttl(name: &'static str) -> bool {
    DEFAULTS
        .iter()
        .find(|default| default.cf == name)
        .is_some_and(|default| {
            matches!(default.ttl, RetentionTtl::Hours(_) | RetentionTtl::Days(_))
        })
}

fn apply_block_cache(options: &mut Options) {
    let cache = Cache::new_lru_cache(BLOCK_CACHE_BYTES);
    let mut block_options = BlockBasedOptions::default();
    block_options.set_block_cache(&cache);
    options.set_block_based_table_factory(&block_options);
}

fn verify_schema_version(db: &DB, path: &Path, schema_version: u32) -> StorageResult<()> {
    db.get(SCHEMA_VERSION_KEY)
        .map_err(|source| open_failed(path, &source))?
        .map_or_else(
            || {
                db.put(SCHEMA_VERSION_KEY, schema_version.to_be_bytes())
                    .map_err(|source| open_failed(path, &source))
            },
            |value| {
                let actual = decode_schema_version(&value);
                if actual == Some(schema_version) {
                    Ok(())
                } else {
                    Err(StorageError::SchemaMismatch {
                        expected: schema_version,
                        actual: actual.unwrap_or_default(),
                    })
                }
            },
        )
}

fn decode_schema_version(value: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = value.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn open_failed(path: &Path, source: &rocksdb::Error) -> StorageError {
    open_failed_detail(path, source.to_string())
}

fn open_failed_detail(path: &Path, detail: String) -> StorageError {
    open_failed_detail_with_backend(path, StorageBackendKind::RocksDb, detail)
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
