pub mod agent_events;
pub mod agent_transcripts;
mod backend;
pub mod cf;
pub mod codecs;
pub mod constellations;
pub mod episodes;
pub mod error;
mod gc;
mod pressure;
pub mod routines;
pub mod timeline;

use std::fmt;
use std::path::{Path, PathBuf};

pub use backend::{
    CalyxVaultCollectionInspect, CalyxVaultInspect, STORAGE_METADATA_ONLY_REDACTION_POLICY,
    StorageBackendKind, StorageCfDump, StorageDumpRow, dump_cf_read_only,
    dump_cf_read_only_with_expired, inspect_calyx_vault_read_only, scan_cf_read_only,
    scan_cf_read_only_with_expired,
};
pub use codecs::{decode_json, encode_json};
pub use constellations::{
    ConstellationPutReport, SYN_ACTION_PANEL_NAME, SYN_ACTION_PANEL_VERSION,
    SYN_AGENT_EVENT_PANEL_NAME, SYN_AGENT_EVENT_PANEL_VERSION, SYN_AGENT_TRANSCRIPT_PANEL_NAME,
    SYN_AGENT_TRANSCRIPT_PANEL_VERSION, SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION,
    SYN_OBSERVATION_PANEL_NAME, SYN_OBSERVATION_PANEL_VERSION,
    SYN_OBSERVATION_SAMPLE_EVERY_N_DEFAULT, SYN_OBSERVATION_SAMPLE_EVERY_N_ENV,
    SYN_PROCESS_PANEL_NAME, SYN_PROCESS_PANEL_VERSION, SYN_REFLEX_PANEL_NAME,
    SYN_REFLEX_PANEL_VERSION, SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION,
};
pub use error::{StorageError, StorageResult};
pub use gc::{GcCfReport, GcReport, GcTask, GcTaskReadback};
pub use pressure::{DiskPressureLevel, PressureProbeReadback, PressureReport, PressureTask};

/// One raw storage row: key bytes and value bytes.
pub type RawRow = (Vec<u8>, Vec<u8>);
/// One column-family batch: CF name plus raw rows.
pub type CfWriteBatch<'a> = (&'a str, Vec<RawRow>);
pub(crate) type OwnedCfWriteBatch = (String, Vec<RawRow>);
/// A bounded scan window plus whether more rows remain past it.
pub type ScanWindow = (Vec<RawRow>, bool);
/// Per-CF integer storage metrics plus CFs whose backend estimate was absent.
pub type CfEstimateMap = (std::collections::BTreeMap<String, u64>, Vec<String>);

/// Opened storage handle.
pub struct Db {
    pub path: PathBuf,
    pub schema_version: u32,
    backend: Box<dyn backend::StorageBackend>,
}

impl fmt::Debug for Db {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Db")
            .field("path", &self.path)
            .field("schema_version", &self.schema_version)
            .field("backend", &self.backend_kind().as_str())
            .finish_non_exhaustive()
    }
}

impl Db {
    /// Opens storage with the default Calyx backend.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::OpenFailed`] when the backend cannot open or
    /// initialize the database, or [`StorageError::SchemaMismatch`] when the
    /// stored schema sentinel differs from `schema_version`.
    #[tracing::instrument(skip_all, fields(storage_path = %path.display(), schema_version))]
    pub fn open(path: &Path, schema_version: u32) -> StorageResult<Self> {
        Self::open_with_backend(path, schema_version, StorageBackendKind::default())
    }

    /// Opens storage with an explicit backend selection.
    ///
    /// # Errors
    ///
    /// Returns a structured open error when the selected backend cannot serve
    /// the `Db` API.
    #[tracing::instrument(skip_all, fields(storage_path = %path.display(), schema_version, backend = backend_kind.as_str()))]
    pub fn open_with_backend(
        path: &Path,
        schema_version: u32,
        backend_kind: StorageBackendKind,
    ) -> StorageResult<Self> {
        let backend: Box<dyn backend::StorageBackend> = match backend_kind {
            StorageBackendKind::Calyx => {
                Box::new(backend::CalyxBackend::open(path, schema_version)?)
            }
        };
        tracing::info!(
            code = "STORAGE_BACKEND_OPENED",
            storage_path = %path.display(),
            backend = backend.kind().as_str(),
            schema_version,
            "storage backend opened"
        );
        Ok(Self {
            path: path.to_path_buf(),
            schema_version,
            backend,
        })
    }

    #[must_use]
    pub fn backend_kind(&self) -> StorageBackendKind {
        self.backend.kind()
    }

    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.backend_kind().as_str()
    }

    /// Enqueues key/value writes for one column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing, the selected
    /// backend rejects the write, or disk-pressure policy sheds the batch.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
    pub fn put_batch<I, K, V>(&self, cf_name: &str, kvs: I) -> StorageResult<()>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        self.backend.put_batch(
            cf_name,
            kvs.into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }

    /// Writes a key/value batch while bypassing the pressure ingestion gate.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing or the
    /// selected backend rejects the batch.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
    pub fn put_batch_pressure_bypass<I, K, V>(&self, cf_name: &str, kvs: I) -> StorageResult<()>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        self.backend.put_batch_pressure_bypass(
            cf_name,
            kvs.into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }

    /// Writes key/value batches across multiple column families atomically
    /// while bypassing the pressure ingestion gate.
    ///
    /// # Errors
    ///
    /// Returns a storage error when any column family is missing or the
    /// selected backend rejects the multi-CF batch/flush.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn put_cf_batches_pressure_bypass(
        &self,
        batches: Vec<CfWriteBatch<'_>>,
    ) -> StorageResult<()> {
        self.backend.put_cf_batches_pressure_bypass(
            batches
                .into_iter()
                .map(|(cf_name, rows)| (cf_name.to_owned(), rows))
                .collect(),
        )
    }

    /// Reads one key from a column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing or the
    /// selected backend rejects the point lookup.
    #[tracing::instrument(skip_all, fields(cf_name, key_len = key.len(), backend = self.backend_name()))]
    pub fn get_cf(&self, cf_name: &str, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        self.backend.get_cf(cf_name, key)
    }

    /// Applies key deletes and key/value writes to one column family atomically.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing or the
    /// selected backend rejects the mutation.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
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
        self.backend.mutate_batch_pressure_bypass(
            cf_name,
            deletes.into_iter().map(Into::into).collect(),
            puts.into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }

    /// Deletes key rows from one column family and flushes them immediately.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing or the
    /// selected backend rejects the delete batch.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
    pub fn delete_batch<I, K>(&self, cf_name: &str, keys: I) -> StorageResult<()>
    where
        I: IntoIterator<Item = K>,
        K: Into<Vec<u8>>,
    {
        self.backend
            .delete_batch(cf_name, keys.into_iter().map(Into::into).collect())
    }

    /// Syncs pending backend writes.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend rejects the flush.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn flush(&self) -> StorageResult<()> {
        self.backend.flush()
    }

    /// Runs one storage garbage-collection pass immediately.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend rejects GC.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn run_gc_once(&self) -> StorageResult<GcReport> {
        self.backend.run_gc_once()
    }

    /// Runs one row-count-scaled GC pass for deterministic local diagnostics.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend rejects GC.
    #[doc(hidden)]
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn run_gc_once_with_row_caps(
        &self,
        cf_name: &'static str,
        soft_cap_rows: u64,
        hard_cap_rows: u64,
    ) -> StorageResult<GcReport> {
        self.backend
            .run_gc_once_with_row_caps(cf_name, soft_cap_rows, hard_cap_rows)
    }

    /// Spawns the periodic storage garbage-collection task.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend cannot spawn GC.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn spawn_gc_task(&self) -> StorageResult<GcTask> {
        self.backend.spawn_gc_task()
    }

    /// Returns the current DB-volume disk-pressure level.
    #[must_use]
    pub fn pressure_level(&self) -> DiskPressureLevel {
        self.backend.pressure_level()
    }

    /// Returns whether the current pressure policy permits writes to `cf_name`.
    #[must_use]
    pub fn pressure_permits_write(&self, cf_name: &str) -> bool {
        self.backend.pressure_permits_write(cf_name)
    }

    /// Returns the in-process disk-pressure transition code history.
    ///
    /// # Errors
    ///
    /// Returns a storage error if pressure state cannot be read.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn pressure_transition_codes(&self) -> StorageResult<Vec<&'static str>> {
        self.backend.pressure_transition_codes()
    }

    /// Returns the last successfully observed disk-pressure probe readback.
    ///
    /// # Errors
    ///
    /// Returns a storage error if pressure state cannot be read.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn pressure_probe_readback(&self) -> StorageResult<PressureProbeReadback> {
        self.backend.pressure_probe_readback()
    }

    /// Returns approximate logical bytes currently stored in each column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when a column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn cf_sizes(&self) -> StorageResult<std::collections::BTreeMap<String, u64>> {
        self.backend.cf_sizes()
    }

    /// Returns backend metadata-backed live-data-size estimates.
    ///
    /// # Errors
    ///
    /// Returns a storage error when a backend estimate cannot be read.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn cf_live_data_size_estimates(&self) -> StorageResult<CfEstimateMap> {
        self.backend.cf_live_data_size_estimates()
    }

    /// Returns exact row counts for each column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when a column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn cf_row_counts(&self) -> StorageResult<std::collections::BTreeMap<String, u64>> {
        self.backend.cf_row_counts()
    }

    /// Returns backend metadata-backed row-count estimates.
    ///
    /// # Errors
    ///
    /// Returns a storage error when a backend estimate cannot be read.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn cf_estimated_row_counts(&self) -> StorageResult<CfEstimateMap> {
        self.backend.cf_estimated_row_counts()
    }

    /// Returns physical Calyx vault collection statistics.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the Calyx vault cannot be inspected.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn calyx_vault_inspect(&self) -> StorageResult<Option<CalyxVaultInspect>> {
        self.backend.calyx_vault_inspect()
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_TIMELINE` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_timeline_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &synapse_core::types::TimelineRecord,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_timeline_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_EPISODES` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_episode_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &synapse_core::types::EpisodeRecord,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_episode_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores native Calyx constellations for persisted
    /// `CF_EPISODES` rows in one backend batch.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(row_count = rows.len(), backend = self.backend_name()))]
    pub fn put_episode_constellations(
        &self,
        rows: &[(Vec<u8>, Vec<u8>, synapse_core::types::EpisodeRecord)],
    ) -> StorageResult<Vec<ConstellationPutReport>> {
        self.backend.put_episode_constellations(rows)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_AGENT_EVENTS` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_agent_event_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &synapse_core::types::AgentEventRecord,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_agent_event_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_AGENT_TRANSCRIPTS` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_agent_transcript_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &synapse_core::types::AgentTranscriptRecord,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_agent_transcript_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_ACTION_LOG` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_action_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &serde_json::Value,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_action_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_REFLEX_AUDIT` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_reflex_audit_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &synapse_core::StoredReflexAudit,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_reflex_audit_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_PROCESS_HISTORY` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when Syn* lens measurement, native Calyx
    /// validation, duplicate compatibility, ledger append, or Base/Slot/Scalars
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_process_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &serde_json::Value,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_process_constellation(source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx constellation for one persisted
    /// `CF_OBSERVATIONS` row when its key is selected by the deterministic
    /// bounded-rate sampler.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the sampler config/key is invalid or Syn*
    /// lens measurement, native Calyx validation, duplicate compatibility,
    /// ledger append, or Base/Slot/Scalars persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_sampled_observation_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &synapse_core::StoredObservation,
    ) -> StorageResult<Option<ConstellationPutReport>> {
        self.backend
            .put_sampled_observation_constellation(source_key, raw_bytes, record)
    }

    /// Runs one disk-pressure check immediately.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend rejects the check.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn run_pressure_check_once(&self) -> StorageResult<PressureReport> {
        self.backend.run_pressure_check_once(&self.path)
    }

    /// Applies one synthetic free-byte sample through the pressure responder.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend rejects the check.
    #[doc(hidden)]
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn run_pressure_check_with_free_bytes_sample(
        &self,
        free_bytes: u64,
    ) -> StorageResult<PressureReport> {
        self.backend
            .run_pressure_check_with_free_bytes_sample(free_bytes)
    }

    /// Spawns the periodic DB-volume disk-pressure task.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend cannot spawn pressure monitoring.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn spawn_pressure_task(&self) -> StorageResult<PressureTask> {
        self.backend.spawn_pressure_task(&self.path)
    }

    /// Scans a column family into owned key/value bytes.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
    pub fn scan_cf(&self, cf_name: &str) -> StorageResult<Vec<RawRow>> {
        self.backend.scan_cf(cf_name)
    }

    /// Scans a column family from a key prefix into owned key/value bytes.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(cf_name, prefix_len = prefix.len(), backend = self.backend_name()))]
    pub fn scan_cf_prefix(&self, cf_name: &str, prefix: &[u8]) -> StorageResult<Vec<RawRow>> {
        self.backend.scan_cf_prefix(cf_name, prefix)
    }

    /// Scans a column family from `start_key` while rows still match `prefix`.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(cf_name, prefix_len = prefix.len(), start_key_len = start_key.len(), backend = self.backend_name()))]
    pub fn scan_cf_prefix_from(
        &self,
        cf_name: &str,
        prefix: &[u8],
        start_key: &[u8],
    ) -> StorageResult<Vec<RawRow>> {
        self.backend.scan_cf_prefix_from(cf_name, prefix, start_key)
    }

    /// Scans up to `max_rows` rows starting at `start_key` (inclusive) and
    /// reports whether more rows remain past the returned window.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(cf_name, start_key_len = start_key.len(), max_rows, backend = self.backend_name()))]
    pub fn scan_cf_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        self.backend.scan_cf_from(cf_name, start_key, max_rows)
    }

    /// Scans up to `max_rows` rows in `[start_key, end_key)`.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family cannot be scanned or the
    /// range is invalid for the backend.
    #[tracing::instrument(skip_all, fields(cf_name, start_key_len = start_key.len(), end_key_len = end_key.len(), max_rows, backend = self.backend_name()))]
    pub fn scan_cf_range(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_rows: usize,
    ) -> StorageResult<ScanWindow> {
        self.backend
            .scan_cf_range(cf_name, start_key, end_key, max_rows)
    }

    /// Scans up to `max_rows` rows from the end of one column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family cannot be scanned.
    #[tracing::instrument(skip_all, fields(cf_name, max_rows, backend = self.backend_name()))]
    pub fn scan_cf_tail(&self, cf_name: &str, max_rows: usize) -> StorageResult<Vec<RawRow>> {
        self.backend.scan_cf_tail(cf_name, max_rows)
    }

    /// Compacts a whole column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
    pub fn compact_cf(&self, cf_name: &str) -> StorageResult<()> {
        self.backend.compact_cf(cf_name)
    }

    /// Compacts one key range of a column family.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family is missing.
    #[tracing::instrument(skip_all, fields(cf_name, start_len = start.len(), end_len = end.len(), backend = self.backend_name()))]
    pub fn compact_cf_range(&self, cf_name: &str, start: &[u8], end: &[u8]) -> StorageResult<()> {
        self.backend.compact_cf_range(cf_name, start, end)
    }
}
