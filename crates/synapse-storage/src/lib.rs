pub mod agent_events;
pub mod agent_transcripts;
mod batch;
pub mod cf;
pub mod codecs;
pub mod compaction;
pub mod episodes;
pub mod error;
mod gc;
mod pressure;
pub mod routines;
pub mod timeline;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, ColumnFamilyRef, DB, DBCompressionType,
    Direction, IteratorMode, Options, SliceTransform, WriteBatch,
};
use synapse_core::{
    error_codes,
    retention::{DEFAULTS, RetentionTtl},
};

pub use codecs::{decode_json, encode_json};
pub use error::{StorageError, StorageResult};
pub use gc::{GcCfReport, GcReport, GcTask, GcTaskReadback};
pub use pressure::{DiskPressureLevel, PressureProbeReadback, PressureReport, PressureTask};

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

/// One raw storage row: key bytes and value bytes.
pub type RawRow = (Vec<u8>, Vec<u8>);
/// One column-family batch: CF name plus raw rows.
pub type CfWriteBatch<'a> = (&'a str, Vec<RawRow>);
/// A bounded scan window plus whether more rows remain past it.
pub type ScanWindow = (Vec<RawRow>, bool);
/// Per-CF integer storage metrics plus CFs whose `RocksDB` property was absent.
pub type CfEstimateMap = (BTreeMap<String, u64>, Vec<String>);

/// Opened storage handle.
pub struct Db {
    pub path: PathBuf,
    pub schema_version: u32,
    batcher: batch::Batcher,
    inner: Arc<DB>,
    pressure: Arc<pressure::PressureState>,
}

impl fmt::Debug for Db {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Db")
            .field("path", &self.path)
            .field("schema_version", &self.schema_version)
            .finish_non_exhaustive()
    }
}

impl Db {
    /// Opens the `RocksDB` storage at `path`.
    ///
    /// `RocksDB` refuses to open a database without naming every column
    /// family that physically exists, so a binary older than the newest CF
    /// would brick the daemon after any rollback (observed live 2026-06-12:
    /// a pre-`CF_ROUTINES` binary died at startup with `Column families not
    /// opened: CF_ROUTINES`). Unknown on-disk CFs are therefore opened with
    /// default options and a loud structured warning: their rows are
    /// preserved untouched for the newer binary that owns them, and the
    /// schema-version sentinel still rejects genuinely incompatible layouts.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::OpenFailed`] when `RocksDB` cannot open or
    /// initialize the database, or [`StorageError::SchemaMismatch`] when the
    /// stored schema sentinel differs from `schema_version`.
    #[tracing::instrument(skip_all, fields(storage_path = %path.display(), schema_version))]
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
            path: path.to_path_buf(),
            schema_version,
            batcher,
            inner,
            pressure,
        })
    }

    /// Enqueues key/value writes for one column family.
    ///
    /// Callers aggregate producer-side and submit batches; per-frame single
    /// writes intentionally are not part of this API.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when the column family is missing,
    /// the background batcher is unavailable, or `RocksDB` rejects the batch.
    /// Returns [`StorageError::WriteShed`] when disk-pressure policy rejects
    /// the write before it reaches `RocksDB`.
    #[tracing::instrument(skip_all, fields(cf_name))]
    pub fn put_batch<I, K, V>(&self, cf_name: &str, kvs: I) -> StorageResult<()>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        self.inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        let kvs = kvs
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        if kvs.is_empty() {
            return Ok(());
        }
        if !self.pressure.permits_write(cf_name) {
            // Consumers like the activity timeline mine continuity and need
            // a returned failure to detect recording gaps (ADR
            // 2026-06-11-timeline-data-model). Logs/metrics are only
            // supporting signals.
            let pressure_level = format!("{:?}", self.pressure.level());
            synapse_telemetry::metrics::counter!(
                STORAGE_WRITES_SHED_TOTAL,
                "cf" => cf_name.to_owned()
            )
            .increment(kvs.len() as u64);
            tracing::warn!(
                code = error_codes::STORAGE_WRITE_FAILED,
                cf = cf_name,
                pressure_level = ?self.pressure.level(),
                dropped_rows = kvs.len(),
                metric_name = STORAGE_WRITES_SHED_TOTAL,
                "storage write dropped under disk pressure"
            );
            return Err(StorageError::WriteShed {
                cf_name: cf_name.to_owned(),
                pressure_level,
                rows: kvs.len(),
            });
        }
        self.batcher.put_batch(cf_name, kvs)
    }

    /// Writes a key/value batch while bypassing the pressure ingestion gate.
    ///
    /// This is reserved for bounded storage-maintenance rewrites, such as
    /// retention backfills and durable maintenance reports. Normal data
    /// ingestion must use [`Self::put_batch`] so disk-pressure policy can
    /// shed non-critical writes.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when the column family is missing
    /// or `RocksDB` rejects the write batch.
    #[tracing::instrument(skip_all, fields(cf_name))]
    pub fn put_batch_pressure_bypass<I, K, V>(&self, cf_name: &str, kvs: I) -> StorageResult<()>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let cf = self
            .inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        let kvs = kvs
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        if kvs.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::default();
        for (key, value) in kvs {
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

    /// Writes key/value batches across multiple column families in one `RocksDB`
    /// write batch while bypassing the pressure ingestion gate.
    ///
    /// Callers must perform any required pressure admission check before using
    /// this helper. It exists for coupled rows where a primary row and its
    /// secondary index must appear atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when any column family is missing
    /// or `RocksDB` rejects the multi-CF write batch/flush.
    #[tracing::instrument(skip_all)]
    pub fn put_cf_batches_pressure_bypass(
        &self,
        batches: Vec<CfWriteBatch<'_>>,
    ) -> StorageResult<()> {
        if batches.iter().all(|(_cf_name, rows)| rows.is_empty()) {
            return Ok(());
        }
        let mut handles = Vec::with_capacity(batches.len());
        for (cf_name, _rows) in &batches {
            let handle =
                self.inner
                    .cf_handle(cf_name)
                    .ok_or_else(|| StorageError::WriteFailed {
                        cf_name: (*cf_name).to_owned(),
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
                    cf_name: cf_name.to_owned(),
                    detail: source.to_string(),
                })?;
        }
        Ok(())
    }

    /// Reads one key from a column family.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing or
    /// `RocksDB` rejects the point lookup.
    #[tracing::instrument(skip_all, fields(cf_name, key_len = key.len()))]
    pub fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        let handle = self.cf_handle(cf_name)?;
        self.inner
            .get_cf(&handle, key)
            .map_err(|source| StorageError::ReadFailed {
                cf_name: cf_name.to_owned(),
                detail: source.to_string(),
            })
    }

    /// Applies key deletes and key/value writes to one column family in a
    /// single synchronous `RocksDB` batch while bypassing pressure shedding.
    ///
    /// This is reserved for bounded coordination-state rewrites where readers
    /// must never observe a release gap between deleting one owner row and
    /// writing another.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when the column family is missing
    /// or `RocksDB` rejects the write batch.
    #[tracing::instrument(skip_all, fields(cf_name))]
    pub fn mutate_batch_pressure_bypass<D, K, P, PK, PV>(
        &self,
        cf_name: &str,
        deletes: D,
        puts: P,
    ) -> StorageResult<()>
    where
        D: IntoIterator<Item = K>,
        K: Into<Vec<u8>>,
        P: IntoIterator<Item = (PK, PV)>,
        PK: Into<Vec<u8>>,
        PV: Into<Vec<u8>>,
    {
        let cf = self
            .inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        let deletes = deletes.into_iter().map(Into::into).collect::<Vec<_>>();
        let puts = puts
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
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

    /// Deletes key rows from one column family and flushes them immediately.
    ///
    /// Deletions are allowed under disk pressure because they reduce retained
    /// local state.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when the column family is missing
    /// or `RocksDB` rejects the delete batch.
    #[tracing::instrument(skip_all, fields(cf_name))]
    pub fn delete_batch<I, K>(&self, cf_name: &str, keys: I) -> StorageResult<()>
    where
        I: IntoIterator<Item = K>,
        K: Into<Vec<u8>>,
    {
        let cf = self
            .inner
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::WriteFailed {
                cf_name: cf_name.to_owned(),
                detail: "column family handle missing".to_owned(),
            })?;
        let keys = keys.into_iter().map(Into::into).collect::<Vec<_>>();
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

    /// Syncs the batcher WAL. `put_batch` already waits for each submitted
    /// batch to reach `RocksDB` with a synced WAL before returning.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when the background batcher is
    /// unavailable or `RocksDB` rejects the flush.
    #[tracing::instrument(skip_all)]
    pub fn flush(&self) -> StorageResult<()> {
        self.batcher.flush()
    }

    /// Runs one storage garbage-collection pass immediately.
    ///
    /// # Errors
    ///
    /// Returns a storage error when `RocksDB` property reads, deletes, flushes,
    /// or compactions fail.
    #[tracing::instrument(skip_all)]
    pub fn run_gc_once(&self) -> StorageResult<GcReport> {
        gc::run_once(&self.inner, &gc::GcConfig::from_retention_defaults())
    }

    /// Runs one row-count-scaled GC pass for deterministic regression tests.
    ///
    /// This avoids writing gigabytes to hit production byte caps.
    ///
    /// # Errors
    ///
    /// Returns a storage error when `RocksDB` property reads, deletes, flushes,
    /// or compactions fail.
    #[doc(hidden)]
    #[tracing::instrument(skip_all)]
    pub fn run_gc_once_with_row_caps(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<GcReport> {
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

    /// Spawns the periodic storage garbage-collection task.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when no Tokio runtime is active.
    #[tracing::instrument(skip_all)]
    pub fn spawn_gc_task(&self) -> StorageResult<GcTask> {
        gc::spawn(
            Arc::clone(&self.inner),
            gc::GcConfig::from_retention_defaults(),
        )
    }

    /// Returns the current DB-volume disk-pressure level.
    #[must_use]
    pub fn pressure_level(&self) -> DiskPressureLevel {
        self.pressure.level()
    }

    /// Returns whether the current pressure policy permits writes to `cf_name`.
    #[must_use]
    pub fn pressure_permits_write(&self, cf_name: &str) -> bool {
        self.pressure.permits_write(cf_name)
    }

    /// Returns the in-process disk-pressure transition code history.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] if the pressure state cannot be read.
    #[tracing::instrument(skip_all)]
    pub fn pressure_transition_codes(&self) -> StorageResult<Vec<&'static str>> {
        self.pressure.transition_codes()
    }

    /// Returns the last successfully observed disk-pressure probe readback.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] if the pressure state cannot be read.
    #[tracing::instrument(skip_all)]
    pub fn pressure_probe_readback(&self) -> StorageResult<PressureProbeReadback> {
        self.pressure.probe_readback()
    }

    /// Returns approximate logical bytes currently stored in each Synapse column family.
    ///
    /// This scans row keys and values so health reports reflect persisted data
    /// even when `RocksDB` has not compacted files yet.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when a column family cannot be scanned.
    #[tracing::instrument(skip_all)]
    pub fn cf_sizes(&self) -> StorageResult<BTreeMap<String, u64>> {
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

    /// Returns `RocksDB`'s live-data-size estimate for each Synapse column family.
    ///
    /// This is metadata-backed and intentionally cheaper than [`Self::cf_sizes`],
    /// which scans every key/value byte to compute exact logical sizes.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when a column family property cannot
    /// be read.
    #[tracing::instrument(skip_all)]
    pub fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap> {
        let estimates = self.cf_property_estimates(ESTIMATE_LIVE_DATA_SIZE)?;
        emit_storage_cf_bytes(&estimates.0);
        Ok(estimates)
    }

    /// Returns exact row counts for each Synapse column family.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when a column family cannot be scanned.
    #[tracing::instrument(skip_all)]
    pub fn cf_row_counts(&self) -> StorageResult<BTreeMap<String, u64>> {
        let mut counts = BTreeMap::new();
        for cf_name in cf::ALL_COLUMN_FAMILIES {
            counts.insert(cf_name.to_owned(), self.scan_cf(cf_name)?.len() as u64);
        }
        Ok(counts)
    }

    /// Returns `RocksDB`'s estimated row count for each Synapse column family.
    ///
    /// This is metadata-backed and intentionally cheaper than [`Self::cf_row_counts`],
    /// which scans every key in every column family.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when a column family property cannot
    /// be read.
    #[tracing::instrument(skip_all)]
    pub fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap> {
        self.cf_property_estimates(ESTIMATE_NUM_KEYS)
    }

    /// Runs one disk-pressure check immediately.
    ///
    /// # Errors
    ///
    /// Returns a storage error when disk free-space probing or pressure-triggered
    /// compaction fails.
    #[tracing::instrument(skip_all)]
    pub fn run_pressure_check_once(&self) -> StorageResult<PressureReport> {
        pressure::run_once(
            &self.inner,
            &self.pressure,
            &self.path,
            &pressure::PressureConfig::default(),
        )
    }

    /// Applies one synthetic free-byte sample for deterministic regression tests.
    ///
    /// This uses the production thresholds and responder actions while avoiding
    /// host-volume manipulation.
    ///
    /// # Errors
    ///
    /// Returns a storage error when pressure-triggered compaction fails.
    #[doc(hidden)]
    #[tracing::instrument(skip_all)]
    pub fn run_pressure_check_with_free_bytes_sample(
        &self,
        free_bytes: u64,
    ) -> StorageResult<PressureReport> {
        pressure::run_once_with_free_bytes(
            &self.inner,
            &self.pressure,
            &pressure::PressureConfig::default(),
            free_bytes,
        )
    }

    /// Spawns the periodic DB-volume disk-pressure task.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::WriteFailed`] when no Tokio runtime is active.
    #[tracing::instrument(skip_all)]
    pub fn spawn_pressure_task(&self) -> StorageResult<PressureTask> {
        pressure::spawn(
            Arc::clone(&self.inner),
            Arc::clone(&self.pressure),
            self.path.clone(),
            pressure::PressureConfig::default(),
        )
    }

    /// Scans a column family into owned key/value bytes.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing
    /// or `RocksDB` iteration fails.
    #[tracing::instrument(skip_all, fields(cf_name))]
    pub fn scan_cf(&self, cf_name: &str) -> StorageResult<Vec<(Vec<u8>, Vec<u8>)>> {
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

    /// Scans a column family from a key prefix into owned key/value bytes.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing
    /// or `RocksDB` iteration fails.
    #[tracing::instrument(skip_all, fields(cf_name, prefix_len = prefix.len()))]
    pub fn scan_cf_prefix(
        &self,
        cf_name: &str,
        prefix: &[u8],
    ) -> StorageResult<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_cf_prefix_from(cf_name, prefix, prefix)
    }

    /// Scans a column family from `start_key` while rows still match `prefix`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing
    /// or `RocksDB` iteration fails.
    #[tracing::instrument(skip_all, fields(cf_name, prefix_len = prefix.len(), start_key_len = start_key.len()))]
    pub fn scan_cf_prefix_from(
        &self,
        cf_name: &str,
        prefix: &[u8],
        start_key: &[u8],
    ) -> StorageResult<Vec<(Vec<u8>, Vec<u8>)>> {
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

    /// Scans up to `max_rows` rows starting at `start_key` (inclusive) and
    /// reports whether more rows remain past the returned window.
    ///
    /// Unlike [`Self::scan_cf_prefix_from`], iteration stops after
    /// `max_rows + 1` steps, so callers can page through arbitrarily large
    /// column families without materializing them.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing
    /// or `RocksDB` iteration fails.
    #[tracing::instrument(skip_all, fields(cf_name, start_key_len = start_key.len(), max_rows))]
    pub fn scan_cf_from(
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

    /// Scans up to `max_rows` rows from the end of one column family.
    ///
    /// Returned rows preserve normal ascending key order, matching the tail
    /// produced by a full forward scan without materializing the whole CF.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing
    /// or `RocksDB` iteration fails.
    #[tracing::instrument(skip_all, fields(cf_name, max_rows))]
    pub fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>> {
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

    /// Compacts a whole column family.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing.
    #[tracing::instrument(skip_all, fields(cf_name))]
    pub fn compact_cf(&self, cf_name: &str) -> StorageResult<()> {
        let handle = self.cf_handle(cf_name)?;
        self.inner
            .compact_range_cf(&handle, None::<&[u8]>, None::<&[u8]>);
        Ok(())
    }

    /// Compacts one key range of a column family.
    ///
    /// `RocksDB`'s documented remedy for tombstone buildup after a bulk
    /// scan-and-delete is `CompactRange` over exactly the deleted range, so
    /// space is reclaimed and iterators do not slow down on tombstone runs
    /// (ADR 2026-06-11-timeline-data-model §6, purge mechanics).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ReadFailed`] when the column family is missing.
    #[tracing::instrument(skip_all, fields(cf_name, start_len = start.len(), end_len = end.len()))]
    pub fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()> {
        let handle = self.cf_handle(cf_name)?;
        self.inner.compact_range_cf(&handle, Some(start), Some(end));
        Ok(())
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

#[allow(clippy::cast_precision_loss)]
fn emit_storage_cf_bytes(sizes: &BTreeMap<String, u64>) {
    for (cf_name, bytes) in sizes {
        synapse_telemetry::metrics::gauge!(STORAGE_CF_BYTES, "cf" => cf_name.clone())
            .set(*bytes as f64);
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
            // Long-TTL rows live in cold SST files that normal write churn
            // never compacts; force every file through the TTL compaction
            // filter at least daily (ADR 2026-06-11-timeline-data-model;
            // CF_EPISODES shares the long-retention profile, #846;
            // CF_AGENT_EVENTS keys/expires the same way, #897).
            options.set_periodic_compaction_seconds(TIMELINE_PERIODIC_COMPACTION_SECONDS);
        }
        cf::CF_AGENT_TRANSCRIPTS => {
            // Same long-retention TTL profile as CF_AGENT_EVENTS (#900), but
            // keys are spawn-id-prefixed (variable length), so no fixed
            // 8-byte prefix extractor applies.
            options.set_compression_type(DBCompressionType::Zstd);
            options.set_periodic_compaction_seconds(TIMELINE_PERIODIC_COMPACTION_SECONDS);
        }
        _ => {}
    }

    if cf_has_ttl(name) {
        // RocksDB TTL filters only remove expired entries during compaction.
        // Apply periodic compaction to every TTL-backed CF so cold files still
        // traverse the filter without a foreground full-CF scan.
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
    tracing::warn!(
        code = error_codes::STORAGE_OPEN_FAILED,
        storage_path = %path.display(),
        %detail,
        "storage open failed"
    );
    StorageError::OpenFailed {
        path: path.to_path_buf(),
        detail,
    }
}
