pub mod action_log;
pub mod agent_events;
pub mod agent_transcripts;
mod backend;
pub mod cf;
pub mod codecs;
pub mod constellations;
pub mod derived_state;
pub mod episodes;
pub mod error;
mod gc;
pub mod maintenance;
pub mod panel_coverage;
mod pressure;
pub mod routines;
pub mod search_sweep;
pub mod timeline;

use std::fmt;
use std::path::{Path, PathBuf};

pub use backend::{
    ActionOraclePublicationReport, CalyxAnchorBatchWriteReport, CalyxAnchorRow,
    CalyxAnchorScanReport, CalyxAnchorValueReadback, CalyxAnchorWriteReport,
    CalyxRecurrenceSubjectReport, CalyxVaultCollectionInspect, CalyxVaultInspect, GroundingAnchor,
    GroundingAnchorSource, GroundingAnchorValue, McpUsageGroundedPublicationReport,
    PanelLifecycleBackfillReport, STORAGE_METADATA_ONLY_REDACTION_POLICY, StorageBackendKind,
    StorageCfDump, StorageDumpRow, SynapseAnchorSourceCarrier, SynapseSynergyPair,
    SynapseSynergyReport, dump_cf_read_only, dump_cf_read_only_with_expired,
    inspect_calyx_vault_read_only, scan_cf_read_only, scan_cf_read_only_with_expired,
};
pub use codecs::{decode_json, encode_json};
pub use constellations::{
    ConstellationPutReport, RecurrenceSubjectKind, SYN_ACTION_PANEL_NAME, SYN_ACTION_PANEL_VERSION,
    SYN_AGENT_EVENT_PANEL_NAME, SYN_AGENT_EVENT_PANEL_VERSION, SYN_AGENT_TRANSCRIPT_PANEL_NAME,
    SYN_AGENT_TRANSCRIPT_PANEL_VERSION, SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION,
    SYN_MCP_USAGE_KEY_PREFIX, SYN_MCP_USAGE_PANEL_NAME, SYN_MCP_USAGE_PANEL_VERSION,
    SYN_OBSERVATION_PANEL_NAME, SYN_OBSERVATION_PANEL_VERSION,
    SYN_OBSERVATION_SAMPLE_EVERY_N_DEFAULT, SYN_OBSERVATION_SAMPLE_EVERY_N_ENV,
    SYN_OUTCOME_PANEL_NAME, SYN_OUTCOME_PANEL_VERSION, SYN_PROCESS_PANEL_NAME,
    SYN_PROCESS_PANEL_VERSION, SYN_RECURRENCE_SUBJECT_PANEL_NAME,
    SYN_RECURRENCE_SUBJECT_PANEL_VERSION, SYN_REFLEX_PANEL_NAME, SYN_REFLEX_PANEL_VERSION,
    SYN_TIMELINE_PANEL_NAME, SYN_TIMELINE_PANEL_VERSION,
};
pub use error::{
    STORAGE_REVISION_GUARD_INVALID, STORAGE_REVISION_GUARDED_BATCH_TOO_LARGE,
    STORAGE_REVISION_GUARDED_OUTCOME_INVALID, StorageError, StorageResult,
};
pub use gc::{
    DerivedSourceCensus, GcCfReport, GcReport, GcTask, GcTaskReadback,
    maintenance_failure_classification,
};
pub use pressure::{DiskPressureLevel, PressureProbeReadback, PressureReport, PressureTask};
pub use synapse_calyx::timeseries::{SynapseCalyxRollupValue, SynapseCalyxRollupWindow};

/// One raw storage row: key bytes and value bytes.
pub type RawRow = (Vec<u8>, Vec<u8>);

/// One physical Calyx envelope revision and its logical payload state.
///
/// The outer `Option` returned by [`Db::get_cf_revisioned`] is `None` only
/// when the physical row is absent/tombstoned. `value` is `None` when the
/// physical retention envelope exists but is logically expired, allowing a
/// caller to guard or delete that exact stale revision without exposing it as
/// a live logical value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionedRawValue {
    pub value: Option<Vec<u8>>,
    pub revision_sha256: [u8; 32],
}

/// One logical same-CF revision precondition.
///
/// `expected_revision_sha256` is SHA-256 over the exact physical Calyx value
/// envelope returned by [`Db::get_cf_revisioned`], including an expired
/// envelope whose logical `value` is `None`, or `None` when the physical row
/// must be absent. Guards are evaluated in input order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionGuard {
    pub key: Vec<u8>,
    pub expected_revision_sha256: Option<[u8; 32]>,
}

/// One logical cross-CF revision precondition.
///
/// The logical column-family name is part of the guarded identity. This is
/// used when facts that live in different Synapse collections must share one
/// physical Calyx WAL/MVCC commit (for example an append-only journal row and
/// its durable projection intent).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CfRevisionGuard {
    pub cf_name: String,
    pub key: Vec<u8>,
    pub expected_revision_sha256: Option<[u8; 32]>,
}

impl CfRevisionGuard {
    #[must_use]
    pub fn new(
        cf_name: impl Into<String>,
        key: impl Into<Vec<u8>>,
        expected_revision_sha256: Option<[u8; 32]>,
    ) -> Self {
        Self {
            cf_name: cf_name.into(),
            key: key.into(),
            expected_revision_sha256,
        }
    }
}

impl RevisionGuard {
    #[must_use]
    pub fn new(key: impl Into<Vec<u8>>, expected_revision_sha256: Option<[u8; 32]>) -> Self {
        Self {
            key: key.into(),
            expected_revision_sha256,
        }
    }
}

/// The first ordered logical revision guard that conflicted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionGuardConflict {
    pub guard_index: usize,
    pub key: Vec<u8>,
    pub expected_revision_sha256: Option<[u8; 32]>,
    pub actual_revision_sha256: Option<[u8; 32]>,
}

/// Outcome of one atomic same-CF guarded mutation containing puts and deletes.
///
/// `actual_revisions_sha256` always follows guard-input order. On success,
/// `committed_revisions_sha256` has the same length and uses `None` for a
/// deleted guard. On conflict, no logical or physical row is mutated,
/// `committed_revisions_sha256` is empty, and `conflict` identifies the first
/// mismatching guard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionGuardedMutationOutcome {
    pub applied: bool,
    pub committed_seq: u64,
    pub actual_revisions_sha256: Vec<Option<[u8; 32]>>,
    pub committed_revisions_sha256: Vec<Option<[u8; 32]>>,
    pub conflict: Option<RevisionGuardConflict>,
}

/// Compatibility outcome of one single-key revision-guarded logical CF batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevisionGuardedWriteOutcome {
    pub applied: bool,
    pub committed_seq: u64,
    pub previous_revision_sha256: Option<[u8; 32]>,
    pub committed_revision_sha256: Option<[u8; 32]>,
}

/// One column-family batch: CF name plus raw rows.
pub type CfWriteBatch<'a> = (&'a str, Vec<RawRow>);
pub(crate) type OwnedCfWriteBatch = (String, Vec<RawRow>);
/// A bounded scan window plus whether more rows remain past it.
pub type ScanWindow = (Vec<RawRow>, bool);
/// Default bounded lifetime for one coherent multi-page enumeration.
pub const COHERENT_SCAN_DEFAULT_MAX_AGE_MS: u64 = 60_000;
/// Hard retention ceiling for a coherent reader lease. Long jobs must rebase
/// explicitly instead of pinning unbounded MVCC history.
pub const COHERENT_SCAN_MAX_AGE_MS: u64 = 5 * 60_000;
/// One candidate-bounded page in a logical column family's physical Calyx
/// namespace.
///
/// `resume_after_physical` is an opaque, exclusive physical Calyx cursor. It
/// includes tombstoned or retention-expired candidates, so callers must pass
/// it back unchanged to the next call for the same column family whenever
/// `more` is true, even when `rows` is empty. It must never be interpreted as
/// a logical Synapse row key or reused with another column family.
///
/// `candidate_rows_examined` counts merged physical-key candidates, including
/// at most one continuation lookahead; it is not an SST row/file/byte count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalScanPage {
    pub rows: Vec<RawRow>,
    pub resume_after_physical: Option<Vec<u8>>,
    pub more: bool,
    pub snapshot_seq: Option<u64>,
    pub candidate_rows_examined: usize,
    pub expired_rows_skipped: usize,
}

impl PhysicalScanPage {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            rows: Vec::new(),
            resume_after_physical: None,
            more: false,
            snapshot_seq: None,
            candidate_rows_examined: 0,
            expired_rows_skipped: 0,
        }
    }
}

/// One candidate-bounded fixed-width scan page.
///
/// `resume_after` is the last logical candidate consumed into this page,
/// including a tombstoned or expired row. Use it as an exclusive cursor
/// whenever `more` is true, even when `rows` is empty.
/// `candidate_rows_examined` counts merged logical candidates, including at
/// most one continuation lookahead; it is not a physical SST row/byte count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixedWidthScanPage {
    pub rows: Vec<RawRow>,
    pub resume_after: Option<Vec<u8>>,
    pub more: bool,
    pub snapshot_seq: Option<u64>,
    pub candidate_rows_examined: usize,
    pub expired_rows_skipped: usize,
}

#[derive(Debug)]
enum CoherentScanScope {
    PhysicalColumnFamily,
    FixedWidthRange {
        start_key: Vec<u8>,
        end_key: Vec<u8>,
    },
}

/// One bounded, single-sequence Calyx scan lease.
///
/// The lease is deliberately stateful and non-cloneable. Continuation is
/// carried inside the handle so callers cannot replay an old cursor, cross a
/// column-family/range boundary, or silently start a later page at a new
/// generation. Release it as soon as enumeration completes; Calyx also
/// enforces `expires_at_unix_ms` when a caller fails to release it.
#[derive(Debug)]
pub struct CoherentScanLease {
    pub lease_id: u64,
    pub snapshot_seq: u64,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub read_at_unix_ms: u64,
    pub cf_name: String,
    snapshot: calyx_aster::mvcc::Snapshot,
    scope: CoherentScanScope,
    next_after: Option<Vec<u8>>,
    started: bool,
    completed: bool,
    released: bool,
}

impl CoherentScanLease {
    /// Returns the exact exclusive cursor that the next page will consume.
    #[must_use]
    pub fn next_after(&self) -> Option<&[u8]> {
        self.next_after.as_deref()
    }

    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.completed
    }

    #[must_use]
    pub const fn is_released(&self) -> bool {
        self.released
    }
}

impl FixedWidthScanPage {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            rows: Vec::new(),
            resume_after: None,
            more: false,
            snapshot_seq: None,
            candidate_rows_examined: 0,
            expired_rows_skipped: 0,
        }
    }
}
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

fn validate_action_log_puts(cf_name: &str, rows: &[RawRow]) -> StorageResult<()> {
    if cf_name != cf::CF_ACTION_LOG {
        return Ok(());
    }
    for (row_index, (key, value)) in rows.iter().enumerate() {
        validate_one_action_log_put(cf_name, row_index, key, value)?;
    }
    Ok(())
}

fn validate_one_action_log_put(
    cf_name: &str,
    row_index: usize,
    key: &[u8],
    value: &[u8],
) -> StorageResult<()> {
    let Err(error) = action_log::validate_action_log_row(key, value) else {
        return Ok(());
    };
    let diagnostic = action_log::diagnostic_for_invalid_row(key, value, &error);
    tracing::error!(
        code = "ACTION_LOG_CODEC_INVALID",
        failure_code = diagnostic.failure_code,
        failure_detail = %diagnostic.failure_detail,
        row_index,
        key_len_bytes = diagnostic.key_len_bytes,
        key_sha256 = %diagnostic.key_sha256,
        value_len_bytes = diagnostic.value_len_bytes,
        value_sha256 = %diagnostic.value_sha256,
        "rejected noncanonical CF_ACTION_LOG write before storage mutation"
    );
    Err(StorageError::WriteFailed {
        cf_name: cf_name.to_owned(),
        detail: format!(
            "ACTION_LOG_CODEC_INVALID failure_code={} row_index={row_index} key_len_bytes={} key_sha256={} value_len_bytes={} value_sha256={} detail={} remediation=encode a schema_version=1 action or command audit row whose 12-byte big-endian timestamp/sequence key exactly matches ts_ns, seq, and audit_id",
            diagnostic.failure_code,
            diagnostic.key_len_bytes,
            diagnostic.key_sha256,
            diagnostic.value_len_bytes,
            diagnostic.value_sha256,
            diagnostic.failure_detail,
        ),
    })
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
        Self::open_with_backend_config(path, schema_version, backend_kind, None)
    }

    /// Opens storage with the exact Calyx configuration already resolved by
    /// the application boundary.
    ///
    /// This is the daemon path: command-line and environment precedence is
    /// resolved once, then passed explicitly so the storage layer cannot
    /// silently re-read a different source.
    ///
    /// # Errors
    ///
    /// Returns a structured open error when the selected backend cannot serve
    /// the `Db` API or the resolved configuration targets a different vault.
    #[tracing::instrument(skip_all, fields(storage_path = %path.display(), schema_version, backend = backend_kind.as_str()))]
    pub fn open_with_resolved_calyx_config(
        path: &Path,
        schema_version: u32,
        backend_kind: StorageBackendKind,
        calyx_config: synapse_calyx::SynapseCalyxConfig,
    ) -> StorageResult<Self> {
        Self::open_with_backend_config(path, schema_version, backend_kind, Some(calyx_config))
    }

    fn open_with_backend_config(
        path: &Path,
        schema_version: u32,
        backend_kind: StorageBackendKind,
        calyx_config: Option<synapse_calyx::SynapseCalyxConfig>,
    ) -> StorageResult<Self> {
        let backend: Box<dyn backend::StorageBackend> = match backend_kind {
            StorageBackendKind::Calyx => Box::new(match calyx_config {
                Some(config) => {
                    backend::CalyxBackend::open_with_resolved_config(path, schema_version, config)?
                }
                None => backend::CalyxBackend::open(path, schema_version)?,
            }),
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
        let rows = kvs
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        validate_action_log_puts(cf_name, &rows)?;
        self.backend.put_batch(cf_name, rows)
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
        let rows = kvs
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        validate_action_log_puts(cf_name, &rows)?;
        self.backend.put_batch_pressure_bypass(cf_name, rows)
    }

    /// Writes key/value batches across multiple column families atomically
    /// while bypassing the pressure ingestion gate.
    ///
    /// # Errors
    ///
    /// Returns a storage error when any column family is missing or the
    /// selected backend rejects the multi-CF batch.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn put_cf_batches_pressure_bypass(
        &self,
        batches: Vec<CfWriteBatch<'_>>,
    ) -> StorageResult<()> {
        for (cf_name, rows) in &batches {
            validate_action_log_puts(cf_name, rows)?;
        }
        self.backend.put_cf_batches_pressure_bypass(
            batches
                .into_iter()
                .map(|(cf_name, rows)| (cf_name.to_owned(), rows))
                .collect(),
        )
    }

    /// Atomically writes rows across logical column families when every
    /// guarded row still has the expected physical Calyx-envelope revision.
    ///
    /// Every guard may identify at most one row in `batches`; a guard with no
    /// matching mutation is an atomic read-only precondition. The complete
    /// mutation must contain at least one row and fit one physical Calyx WAL
    /// record; an oversized request is rejected because splitting would
    /// violate cross-CF atomicity.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error for invalid/duplicate identities,
    /// an oversized atomic batch, or any Calyx admission, WAL, MVCC, or
    /// durability failure. Revision conflicts are non-error outcomes with
    /// `applied = false`.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn put_cf_batches_if_revisions_pressure_bypass(
        &self,
        guards: Vec<CfRevisionGuard>,
        batches: Vec<CfWriteBatch<'_>>,
    ) -> StorageResult<RevisionGuardedMutationOutcome> {
        for (cf_name, rows) in &batches {
            validate_action_log_puts(cf_name, rows)?;
        }
        self.backend.put_cf_batches_if_revisions_pressure_bypass(
            guards,
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

    /// Reads one logical value and its exact physical Calyx-envelope revision.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the column family, point read, envelope, or
    /// clock readback is invalid.
    #[tracing::instrument(skip_all, fields(cf_name, key_len = key.len(), backend = self.backend_name()))]
    pub fn get_cf_revisioned(
        &self,
        cf_name: &str,
        key: &[u8],
    ) -> StorageResult<Option<RevisionedRawValue>> {
        self.backend.get_cf_revisioned(cf_name, key)
    }

    /// Commits one logical CF batch only when the guarded row's physical
    /// revision still matches.
    ///
    /// # Errors
    ///
    /// Returns a storage error for malformed/oversized batches or any Calyx
    /// admission, WAL, MVCC, or durability failure. A concurrent revision
    /// conflict is returned as `applied = false`.
    #[tracing::instrument(skip_all, fields(cf_name, guard_key_len = guard_key.len(), backend = self.backend_name()))]
    pub fn put_batch_if_revision_pressure_bypass<I, K, V>(
        &self,
        cf_name: &str,
        guard_key: &[u8],
        expected_revision_sha256: Option<[u8; 32]>,
        rows: I,
    ) -> StorageResult<RevisionGuardedWriteOutcome>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<Vec<u8>>,
        V: Into<Vec<u8>>,
    {
        let rows = rows
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        validate_action_log_puts(cf_name, &rows)?;
        self.backend.put_batch_if_revision_pressure_bypass(
            cf_name,
            guard_key,
            expected_revision_sha256,
            rows,
        )
    }

    /// Atomically applies same-CF deletes and puts only when every logical
    /// guard's exact physical Calyx-envelope revision still matches.
    ///
    /// Guards and guard keys must be non-empty and unique. Mutation keys must
    /// be unique across `deletes` and `puts`; a guard key may occur at most
    /// once in that combined mutation. A guard without a mutation is an atomic
    /// read-only precondition whose revision remains unchanged. The entire
    /// physical mutation must fit one WAL record; this method rejects batches
    /// that would require chunk splitting. Calyx encodes puts as retention
    /// envelopes and deletes as physical MVCC tombstones before performing one
    /// guarded commit. The durable outcome is returned directly; router/SST
    /// flush is separate maintenance and cannot overwrite an applied result.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::RevisionGuardedMutationFailed`] for malformed
    /// guard/mutation input, oversized atomic batches, or invalid backend
    /// outcome shape. Calyx read, admission, WAL, MVCC, and durability failures
    /// retain their structured backend error codes. Revision
    /// conflicts are non-error outcomes with `applied = false`.
    #[tracing::instrument(skip_all, fields(cf_name, backend = self.backend_name()))]
    pub fn mutate_batch_if_revisions_pressure_bypass<G, D, DK, P, PK, PV>(
        &self,
        cf_name: &str,
        guards: G,
        deletes: D,
        puts: P,
    ) -> StorageResult<RevisionGuardedMutationOutcome>
    where
        G: IntoIterator<Item = RevisionGuard>,
        D: IntoIterator<Item = DK>,
        DK: Into<Vec<u8>>,
        P: IntoIterator<Item = (PK, PV)>,
        PK: Into<Vec<u8>>,
        PV: Into<Vec<u8>>,
    {
        let puts = puts
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        validate_action_log_puts(cf_name, &puts)?;
        self.backend.mutate_batch_if_revisions_pressure_bypass(
            cf_name,
            guards.into_iter().collect(),
            deletes.into_iter().map(Into::into).collect(),
            puts,
        )
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
        let puts = puts
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<Vec<_>>();
        validate_action_log_puts(cf_name, &puts)?;
        self.backend.mutate_batch_pressure_bypass(
            cf_name,
            deletes.into_iter().map(Into::into).collect(),
            puts,
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

    /// Spawns the periodic checkpoint-only maintenance task (2026-07-23
    /// cold-start fix): advances the durable manifest floor every ~30s so a
    /// daemon kill strands seconds — not a 5-minute GC interval — of WAL tail
    /// for restart replay.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend cannot spawn the task.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn spawn_checkpoint_task(&self) -> StorageResult<GcTask> {
        self.backend.spawn_checkpoint_task()
    }

    /// Spawns the unattended derived-state maintainer (#1891, #1894): keeps the
    /// persisted search generation inside its freshness budget and republishes a
    /// measured lens-coverage readback for `health` to raise.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the selected backend cannot spawn the task.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn spawn_derived_state_task(&self) -> StorageResult<GcTask> {
        self.backend.spawn_derived_state_task()
    }

    /// Sweeps **every** published search generation, deciding and building each
    /// one against its own freshness budget (#1938).
    ///
    /// # Errors
    ///
    /// Returns a storage error when the published set cannot be enumerated or
    /// the active panel state cannot be read. A failure maintaining one
    /// generation is recorded on that generation's entry and does not stop the
    /// remaining generations from being maintained.
    pub fn maintain_calyx_search_generation(
        &self,
    ) -> StorageResult<crate::search_sweep::SearchGenerationSweep> {
        self.backend.maintain_calyx_search_generation()
    }

    /// Measures how many lenses each registered panel's records actually carry.
    ///
    /// # Errors
    ///
    /// Returns a storage error when a panel corpus cannot be scanned.
    pub fn measure_calyx_lens_coverage(
        &self,
        max_records: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxLensCoverageStatus> {
        self.backend.measure_calyx_lens_coverage(max_records)
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

    /// Produces a durable, self-verifying online backup of the live Calyx vault
    /// into `<target_root>/vault`, with a hashed manifest sidecar.
    ///
    /// # Errors
    ///
    /// Fails closed on a residency violation, a busy maintenance guard, any copy
    /// error, or a backup that does not pass byte-level restore verification.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn backup_calyx_vault(
        &self,
        target_root: &std::path::Path,
        include_regenerable: bool,
    ) -> StorageResult<synapse_calyx::SynapseCalyxBackupReport> {
        self.backend
            .backup_calyx_vault(target_root, include_regenerable)
    }

    /// Runs the read-only aster restore verifier over a vault directory (a backup
    /// copy or a restored data dir) and returns its byte-level report.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the path is not a readable Aster vault.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn verify_calyx_restore(
        &self,
        vault_path: &std::path::Path,
    ) -> StorageResult<synapse_calyx::SynapseCalyxVerifyReport> {
        self.backend.verify_calyx_restore(vault_path)
    }

    /// Runs the scheduled whole-vault verification against the **live** vault:
    /// the read-only restore verifier plus the provenance hash-chain verifier,
    /// both under the vault maintenance guard so a concurrent backup, erase, or
    /// compaction pass cannot be misreported as corruption. The chain scan is
    /// incremental over the newest `tail_entries` entries unless `full_chain` is
    /// requested. Blocking CPU/IO work: drive it off the async MCP runtime.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the maintenance guard is held, the vault is
    /// unreadable, or the physical ledger cannot be read. A non-green verdict is
    /// a normal report.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn verify_calyx_vault(
        &self,
        full_chain: bool,
        tail_entries: u64,
    ) -> StorageResult<synapse_calyx::SynapseCalyxVaultVerifyReport> {
        self.backend.verify_calyx_vault(full_chain, tail_entries)
    }

    /// Verifies the live provenance-ledger hash chain against the exact stored
    /// bytes, fail-closed. `range` is an optional half-open `(from_seq, to_seq)`
    /// window; `None` verifies the full chain. This is CPU/IO-heavy over the
    /// whole physical Ledger CF and must be driven off the async MCP runtime.
    ///
    /// # Errors
    ///
    /// Returns a storage error only when the physical ledger cannot be read; a
    /// detected tamper is a normal broken/corrupt verdict in the report.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn verify_calyx_ledger_chain(
        &self,
        range: Option<(u64, u64)>,
    ) -> StorageResult<synapse_calyx::SynapseCalyxLedgerVerifyReport> {
        self.backend.verify_calyx_ledger_chain(range)
    }

    /// Reads and decodes one physical provenance-ledger entry by sequence for
    /// provenance readback.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the row cannot be read or decoded.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn read_calyx_ledger_entry(
        &self,
        seq: u64,
    ) -> StorageResult<synapse_calyx::SynapseCalyxLedgerEntryReadback> {
        self.backend.read_calyx_ledger_entry(seq)
    }

    /// Re-derives a record's recorded provenance binding from the bytes and
    /// bounds drift to a genuine, self-consistent ledger entry.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the record is absent or unreadable; a
    /// provenance mismatch is a normal `reproduced == false` verdict.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn reproduce_calyx_record(
        &self,
        cx_id: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxReproduceReport> {
        self.backend.reproduce_calyx_record(cx_id)
    }

    /// Lawfully erases one record by content-addressed id via a ledger-stamped
    /// tombstone, then re-verifies the full chain. Content becomes unrecoverable
    /// at the vault while the chain stays verifiable with the erasure sealed in.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the record is already tombstoned or the
    /// tombstone/commit/purge/re-verify path fails.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn erase_calyx_record(
        &self,
        cx_id: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxErasureReport> {
        self.backend.erase_calyx_record(cx_id)
    }

    /// Appends one physical event to the stable native Calyx recurrence
    /// subject identified by `kind + subject_id`.
    ///
    /// # Errors
    ///
    /// Fails closed for empty/oversized identities, conflicting replay
    /// evidence, invalid event time/context, or any Base/Recurrence commit.
    #[tracing::instrument(skip_all, fields(subject_kind = kind.as_str(), backend = self.backend_name()))]
    pub fn put_recurrence_subject_occurrence(
        &self,
        kind: RecurrenceSubjectKind,
        subject_id: &str,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<CalyxRecurrenceSubjectReport> {
        self.backend.put_recurrence_subject_occurrence(
            kind,
            subject_id,
            event_time_ns,
            occurrence_identity,
            context,
        )
    }

    /// Atomically publishes a terminal action audit source row, its measured
    /// action constellation, and its typed Oracle recurrence occurrence.
    ///
    /// # Errors
    ///
    /// Fails closed before terminal-row visibility when any source identity,
    /// measurement, recurrence, ledger, WAL, or physical-row invariant fails.
    #[tracing::instrument(skip_all, fields(backend = self.backend_name()))]
    pub fn put_action_oracle_publication(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &serde_json::Value,
        event_time_ns: u64,
        occurrence_identity: &[u8],
        context: &[u8],
    ) -> StorageResult<ActionOraclePublicationReport> {
        validate_one_action_log_put(cf::CF_ACTION_LOG, 0, source_key, raw_bytes)?;
        self.backend.put_action_oracle_publication(
            source_key,
            raw_bytes,
            record,
            event_time_ns,
            occurrence_identity,
            context,
        )
    }

    /// Predicts one action's grounded terminal outcome through Calyx Oracle's
    /// persisted sufficiency and provenance gates.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when validation or Oracle prediction fails.
    pub fn oracle_predict_action(&self, action_id: &str) -> StorageResult<serde_json::Value> {
        self.backend.oracle_predict_action(action_id)
    }

    /// Walks backward from a terminal success/failure outcome to grounded
    /// action causes in the persisted Oracle corpus.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the persisted reverse walk fails.
    pub fn oracle_reverse_action(&self, outcome: bool) -> StorageResult<serde_json::Value> {
        self.backend.oracle_reverse_action(outcome)
    }

    /// Completes explicitly selected action slots from grounded persisted peers.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when inputs, evidence, completion, or ledger append fail.
    pub fn oracle_complete_action(
        &self,
        cx_id: &str,
        free_slots: &[u16],
    ) -> StorageResult<serde_json::Value> {
        self.backend.oracle_complete_action(cx_id, free_slots)
    }

    /// Measures and persists the six-tier action readiness report.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when measurement, persistence, or readback fails.
    pub fn oracle_measure_readiness(&self) -> StorageResult<serde_json::Value> {
        self.backend.oracle_measure_readiness()
    }

    /// Measures and persists held-out action validation evidence.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the corpus, measurement, commit, or readback fails.
    pub fn oracle_validate_action(&self) -> StorageResult<serde_json::Value> {
        self.backend.oracle_validate_action()
    }

    /// Reads the latest physically persisted action readiness report.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the report row cannot be read or decoded.
    pub fn oracle_readiness(&self) -> StorageResult<Option<serde_json::Value>> {
        self.backend.oracle_readiness()
    }

    /// Appends a proactive-autonomy decision to the native Calyx Policy ledger.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when validation, encoding, or ledger append fails.
    pub fn append_autonomy_decision(
        &self,
        routine_id: &str,
        decision: &serde_json::Value,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAutonomyDecisionReadback> {
        self.backend.append_autonomy_decision(routine_id, decision)
    }

    /// Reads the authoritative source-row pointer stored on a physical Calyx Base row.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the Base row cannot be read or decoded.
    pub fn read_calyx_base_source_pointer(
        &self,
        cx_id: &str,
    ) -> StorageResult<Option<synapse_calyx::SynapseCalyxBaseSourcePointer>> {
        self.backend.read_calyx_base_source_pointer(cx_id)
    }

    /// Commits one validated recurrence notification intent to Calyx
    /// `Reactive` and independently reads the exact bytes back.
    ///
    /// # Errors
    ///
    /// Fails closed when the outbox write, flush, or physical readback fails.
    pub fn persist_recurrence_finding(
        &self,
        finding: &synapse_calyx::SynapseCalyxPersistedRecurrenceFinding,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPersistedRecurrenceFinding> {
        self.backend.persist_recurrence_finding(finding)
    }

    /// Commits one Ward novelty disposition to Calyx `Reactive` and reads the
    /// exact bytes back before returning it.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when commit, flush, or physical readback fails.
    pub fn persist_novelty_finding(
        &self,
        finding: &synapse_calyx::SynapseCalyxPersistedNoveltyFinding,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPersistedNoveltyFinding> {
        self.backend.persist_novelty_finding(finding)
    }

    /// Reads a bounded prefix of durable Ward-novelty outbox rows.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the range cannot be read or a row is corrupt.
    pub fn persisted_novelty_findings(
        &self,
        after_ledger_seq: u64,
        max_rows: usize,
    ) -> StorageResult<Vec<synapse_calyx::SynapseCalyxPersistedNoveltyFinding>> {
        self.backend
            .persisted_novelty_findings(after_ledger_seq, max_rows)
    }

    /// Reads the durable Ward-novelty delivery cursor.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the Registry row cannot be read or decoded.
    pub fn novelty_delivery_cursor(&self) -> StorageResult<u64> {
        self.backend.novelty_delivery_cursor()
    }

    /// Advances and independently reads back the Ward-novelty delivery cursor.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error on regression or failed commit and readback.
    pub fn persist_novelty_delivery_cursor(&self, ledger_seq: u64) -> StorageResult<u64> {
        self.backend.persist_novelty_delivery_cursor(ledger_seq)
    }

    /// Reads a bounded set of typed exact-identity new-region outbox rows.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the range cannot be read or a row is corrupt.
    pub fn persisted_region_findings(
        &self,
        after_observed_seq: u64,
        max_rows: usize,
    ) -> StorageResult<Vec<synapse_calyx::SynapseCalyxPersistedRegionFinding>> {
        self.backend
            .persisted_region_findings(after_observed_seq, max_rows)
    }

    /// Reads the durable exact-region delivery cursor.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the Registry row cannot be read or decoded.
    pub fn region_delivery_cursor(&self) -> StorageResult<u64> {
        self.backend.region_delivery_cursor()
    }

    /// Advances and independently reads back the exact-region delivery cursor.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error on regression or failed commit and readback.
    pub fn persist_region_delivery_cursor(&self, observed_seq: u64) -> StorageResult<u64> {
        self.backend.persist_region_delivery_cursor(observed_seq)
    }

    /// Reads the physical recurrence series for one stable subject.
    ///
    /// # Errors
    ///
    /// Fails when the subject identity or native recurrence rows are invalid.
    pub fn read_recurrence_subject_series(
        &self,
        kind: RecurrenceSubjectKind,
        subject_id: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxRecurrenceSeriesReadback> {
        self.backend
            .read_recurrence_subject_series(kind, subject_id)
    }

    /// Lists the validated retrieval-only temporal contracts stored in the
    /// native Calyx Registry CF.
    ///
    /// # Errors
    ///
    /// Fails closed when any catalog row is malformed or mis-keyed.
    pub fn list_temporal_panels(
        &self,
    ) -> StorageResult<Vec<synapse_calyx::VaultTemporalPanelRegistration>> {
        self.backend.list_temporal_panels()
    }

    /// Adds one frozen deterministic lens to an active built-in panel and
    /// durably enqueues its existing constellation ids for backfill.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when validation, CAS publication,
    /// backfill enqueue, or physical readback fails.
    pub fn add_panel_lens(
        &self,
        panel_version: u32,
        operation_id: &str,
        slot_key: &str,
        lens_spec: calyx_registry::LensSpec,
        source_projection: synapse_calyx::panel_lifecycle::SynapseCalyxSourceProjection,
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxAddLensReadback> {
        self.backend.add_panel_lens(
            panel_version,
            operation_id,
            slot_key,
            lens_spec,
            source_projection,
        )
    }

    /// Parks, unparks, or irreversibly retires one dynamically added panel slot.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when transition validation, CAS publication, or readback fails.
    pub fn set_panel_lens_state(
        &self,
        panel_version: u32,
        operation_id: &str,
        slot_id: calyx_core::SlotId,
        state: calyx_core::SlotState,
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxSetLensStateReadback> {
        self.backend
            .set_panel_lens_state(panel_version, operation_id, slot_id, state)
    }

    /// Reads the physical Registry CF lifecycle row for one active panel.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when the row cannot be read or decoded.
    pub fn read_panel_lifecycle(
        &self,
        panel_version: u32,
    ) -> StorageResult<Option<synapse_calyx::panel_lifecycle::SynapseCalyxPanelLifecycleState>>
    {
        self.backend.read_panel_lifecycle(panel_version)
    }

    /// Claims, re-measures, physically verifies, and completes one bounded
    /// pressure-aware lifecycle backfill batch.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when claiming, measurement, physical
    /// verification, or durable completion fails.
    pub fn run_panel_backfill(
        &self,
        panel_version: u32,
        limit: usize,
        recover_in_flight: bool,
    ) -> StorageResult<PanelLifecycleBackfillReport> {
        self.backend
            .run_panel_backfill(panel_version, limit, recover_in_flight)
    }

    /// Applies the exact registered Calyx temporal policy to a bounded
    /// content-only candidate set.
    ///
    /// # Errors
    ///
    /// Fails closed for invalid candidates, mixed/unregistered panel
    /// generations, missing event-time evidence, or AP-60 bound violations.
    pub fn temporal_rerank(
        &self,
        candidates: &[synapse_calyx::SynapseCalyxTemporalCandidate],
        query_time_secs: i64,
        tz_offset_secs: i32,
    ) -> StorageResult<synapse_calyx::SynapseCalyxTemporalRerankReadback> {
        self.backend
            .temporal_rerank(candidates, query_time_secs, tz_offset_secs)
    }

    /// Confirms exact-match-by-hash candidates against their authoritative
    /// source fields, so a bucket collision is dropped rather than returned as
    /// an exact match (#1899).
    ///
    /// # Errors
    ///
    /// Fails closed when the panel slot has no declared exact-match lane, a
    /// candidate id is not a `CxId`, a Base row cannot be read, or a source row
    /// cannot be decoded.
    pub fn confirm_exact_matches(
        &self,
        panel_version: u32,
        slot: u16,
        value: &str,
        cx_ids: &[String],
    ) -> StorageResult<Vec<constellations::ExactMatchConfirmation>> {
        self.backend
            .confirm_exact_matches(panel_version, slot, value, cx_ids)
    }

    /// Censuses every panel generation in the `Base` CF and joins it against the
    /// declared panel catalog and its source-CF row counts (#1927 ask 1,
    /// #1920 ask 1).
    ///
    /// One decode-only `Base` scan with no per-slot hydration, plus one row
    /// count per declared full-CF source. This is the cheap sibling of
    /// `hygiene grounding_gap`: it answers "how covered and how grounded is each
    /// panel" without the per-lens hydration that makes `grounding_gap` a heavy
    /// pass, which is what makes it affordable on the derived-state tick.
    ///
    /// Blocking work — admit it off the async runtime workers.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the vault is unavailable, a
    /// source CF cannot be scanned, or the physical row accounting does not add
    /// up (`base_cf_rows != records_total + decode_failures`).
    pub fn measure_panel_coverage(&self) -> StorageResult<panel_coverage::PanelCoverageReport> {
        self.backend.measure_panel_coverage()
    }

    /// Writes one point to a native `TimeSeries` collection.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when collection verification or the write fails.
    pub fn timeseries_write(
        &self,
        collection_name: &str,
        series: u64,
        timestamp_ns: u64,
        value: f64,
    ) -> StorageResult<u64> {
        self.backend
            .timeseries_write(collection_name, series, timestamp_ns, value)
    }

    /// Ensures a native `TimeSeries` collection has the exact immutable descriptor.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error on descriptor conflict or failed create/readback.
    pub fn ensure_timeseries_collection(&self, collection_name: &str) -> StorageResult<()> {
        self.backend.ensure_timeseries_collection(collection_name)
    }

    /// Runs a bounded native OLAP aggregation over one persisted panel slot.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error for invalid bounds or a failed physical scan.
    pub fn olap_aggregate_slot(
        &self,
        panel_version: u32,
        slot_id: u32,
        value_column: usize,
        group_by_column: Option<usize>,
        max_rows: usize,
        max_groups: usize,
    ) -> StorageResult<synapse_calyx::olap::OlapScanResult> {
        self.backend.olap_aggregate_slot(
            panel_version,
            slot_id,
            value_column,
            group_by_column,
            max_rows,
            max_groups,
        )
    }

    /// Reads one native `TimeSeries` rollup bucket.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error on descriptor or rollup-read failure.
    pub fn timeseries_rollup(
        &self,
        collection_name: &str,
        series: u64,
        window: synapse_calyx::timeseries::SynapseCalyxRollupWindow,
        timestamp_ns: u64,
    ) -> StorageResult<Option<synapse_calyx::timeseries::SynapseCalyxRollupValue>> {
        self.backend
            .timeseries_rollup(collection_name, series, window, timestamp_ns)
    }

    /// Reads a timestamp range from one native `TimeSeries` series.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error on descriptor or range-read failure.
    pub fn timeseries_range(
        &self,
        collection_name: &str,
        series: u64,
        start_timestamp_ns: u64,
        end_timestamp_ns: u64,
    ) -> StorageResult<Vec<(u64, f64)>> {
        self.backend.timeseries_range(
            collection_name,
            series,
            start_timestamp_ns,
            end_timestamp_ns,
        )
    }

    /// Reconstructs temporal metadata from authoritative timeline or episode
    /// source rows and atomically journals each changed Base row.
    ///
    /// # Errors
    ///
    /// Fails closed when the source CF/scope is invalid, a source row cannot
    /// be decoded, Base identity differs, or the Base-and-Ledger commit fails.
    pub fn backfill_temporal_metadata(
        &self,
        source_cf: &str,
        source_key: Option<&[u8]>,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<constellations::TemporalMetadataBackfillReport> {
        self.backend
            .backfill_temporal_metadata(source_cf, source_key, after_physical, max_rows)
    }

    /// Returns the status of the exact process-local Calyx vault that owns
    /// Synapse storage and native intelligence rows.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the lifecycle owner is poisoned or closed.
    pub fn calyx_vault_status(&self) -> StorageResult<synapse_calyx::SynapseCalyxVaultStatus> {
        self.backend.calyx_vault_status()
    }

    /// Read-only state of the persisted search generation for the active panel.
    ///
    /// Recall depends entirely on this generation and nothing reported its state
    /// before, so an absent or badly-lagged index was first observed by whoever
    /// called `find` and read the failure (issue #1891).
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the vault handle is closed, or
    /// when the reported state itself would have to be guessed (an unparseable
    /// rebuild marker, an unhashable manifest).
    pub fn calyx_search_generation_status(
        &self,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchGenerationStatus> {
        self.backend.calyx_search_generation_status()
    }

    /// The same state for one **named** panel generation, active or not (#1938).
    ///
    /// `measure_delta = true` scans the `Base` CF and every indexed slot CF; it
    /// is a maintenance-path read, never a request-path one.
    ///
    /// # Errors
    ///
    /// As [`Self::calyx_search_generation_status`], plus a failure of the
    /// changed-key scan when one was requested.
    pub fn calyx_search_generation_status_for_panel(
        &self,
        panel_version: u32,
        measure_delta: bool,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchGenerationStatus> {
        self.backend
            .calyx_search_generation_status_for_panel(panel_version, measure_delta)
    }

    /// One constellation's `Base` and slot row MVCC sequences (#1935).
    ///
    /// # Errors
    ///
    /// Returns a storage error when the row sequences cannot be read.
    pub fn diagnose_constellation_row_sequences(
        &self,
        cx_id: calyx_core::CxId,
    ) -> StorageResult<synapse_calyx::ConstellationRowSequences> {
        self.backend.diagnose_constellation_row_sequences(cx_id)
    }

    /// Keys one native CF's MVCC changed-key history reports after `after_seq`
    /// (#1935).
    ///
    /// # Errors
    ///
    /// Returns a storage error when the CF is unknown or the history cannot
    /// prove the range.
    pub fn calyx_changed_key_count_after(
        &self,
        cf_name: &str,
        after_seq: u64,
    ) -> StorageResult<u64> {
        self.backend
            .calyx_changed_key_count_after(cf_name, after_seq)
    }

    /// Rows one native Calyx CF holds at the latest snapshot (#1935).
    ///
    /// # Errors
    ///
    /// Returns a storage error when the CF is unknown or the scan fails.
    pub fn calyx_cf_row_count(&self, cf_name: &str) -> StorageResult<u64> {
        self.backend.calyx_cf_row_count(cf_name)
    }

    /// Publishes the lowered guard-threshold hot-path artifact (#1686) through
    /// the vault's own producer.
    ///
    /// This exists so there is exactly ONE producer of the fingerprinted
    /// envelope (#1885). The off-tick maintenance publisher previously rebuilt
    /// the same payload, fingerprint and atomic-publish sequence by hand purely
    /// because the `Arc<SynapseCalyxVault>` is private to the backend; two
    /// independent producers of one on-disk format drift silently. Callers must
    /// admit this off the async runtime workers — it is blocking file I/O and
    /// asserts a cold Calyx context.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the vault handle is closed or
    /// poisoned, the frozen payload cannot be encoded, or the atomic publish
    /// fails.
    pub fn lower_guard_thresholds(
        &self,
        params: &synapse_calyx::LoweringParams,
    ) -> StorageResult<synapse_calyx::LoweredPublishReport> {
        self.backend.lower_guard_thresholds(params)
    }

    /// Rebuilds and independently reopens the persisted Calyx search
    /// generation for the exact durable active panel.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the vault is unavailable, the
    /// panel precondition fails, rebuilding fails, or physical artifact
    /// readback cannot validate the published generation.
    pub fn rebuild_calyx_search_indexes(
        &self,
        expected_panel_version: u32,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSearchRebuildReport> {
        self.backend
            .rebuild_calyx_search_indexes(expected_panel_version)
    }

    /// Builds an isolated persisted-search generation, replays real vault
    /// records through candidate and incumbent, then publishes only a passing
    /// tuning+manifest pair.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when build, paired measurement,
    /// promotion, or physical artifact readback fails.
    pub fn propose_calyx_search_tuning(
        &self,
        expected_panel_version: u32,
        candidate: synapse_calyx::SynapseCalyxTuningConfig,
        description: &str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAnnealSearchReport> {
        self.backend
            .propose_calyx_search_tuning(expected_panel_version, candidate, description)
    }

    /// Restores the prior native Anneal artifact and its exact bound search
    /// manifests, with independent physical readback.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when rollback, publication, or readback fails.
    pub fn rollback_calyx_anneal(
        &self,
        change_id: u64,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAnnealRollbackReport> {
        self.backend.rollback_calyx_anneal(change_id)
    }

    /// Runs one fused Calyx find-similar pass (per-slot recall, RRF fusion,
    /// optional bounded temporal boost, agree/disagree evidence) over the
    /// persisted per-slot indexes for the active panel. Read-only; the heavy
    /// index work should be admitted off the runtime workers by the caller.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the vault is unavailable, the
    /// persisted search generation is missing/stale (naming the rebuild
    /// remediation), the example record is absent or on another panel, or the
    /// requested temporal boost cannot be applied.
    pub fn find_similar(
        &self,
        params: &synapse_calyx::SynapseCalyxFindParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxFindReport> {
        self.backend.find_similar(params)
    }

    /// Retires orphaned physical `cf/slot_*` column families that no live panel
    /// references (issue #1776), deriving orphan-ness from live Base membership.
    /// Fail-closed with readback, idempotent, and per-CF durable-lock bounded.
    /// The blocking pass must be admitted off runtime workers by its transport
    /// owner. The MCP storage facade enforces a single-flight permit and
    /// `spawn_blocking`; keeping another backend-local async wrapper created an
    /// unused, competing admission owner (#1835).
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the vault is unavailable, the
    /// legitimate slot set cannot be derived, a candidate still resolves to a
    /// live Base row, or a physical removal/readback fails.
    pub fn retire_orphan_slot_cfs(&self) -> StorageResult<synapse_calyx::AsterOrphanSlotGcReport> {
        self.backend.retire_orphan_slot_cfs()
    }

    /// Retires one closed superseded panel's published search generation
    /// (#1972), returning the removal evidence and the lineage that authorised
    /// it.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the panel version is
    /// maintainable (it has a code-declared contract), when it has no place in
    /// any live panel's declared lineage, when no generation is published for
    /// it, when it is the active panel, or when the removal or its readback
    /// fails.
    pub fn retire_search_generation(
        &self,
        panel_version: u32,
    ) -> StorageResult<(
        synapse_calyx::SynapseCalyxRetiredSearchGeneration,
        crate::constellations::SupersededPanelLineage,
    )> {
        self.backend.retire_search_generation(panel_version)
    }

    /// Weaves the native Loom base associations for one panel: within-record
    /// cross-terms, the slot-pair agreement graph, and the between-record
    /// nearest-neighbor graph, persisted to the native `XTerm`/`Graph` CFs and
    /// read back.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the Base CF cannot be scanned, a
    /// constellation fails to decode, the substrate math rejects a vector, the
    /// math backend is unavailable, or any CF write/readback fails.
    pub fn weave_panel_intelligence(
        &self,
        params: synapse_calyx::SynapseCalyxWeaveParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxWeaveReport> {
        self.backend.weave_panel_intelligence(params)
    }

    /// Reads the derived-data abundance report for one panel back from the
    /// physical `Base`, `XTerm`, and `Graph` CFs without re-weaving.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when any CF cannot be scanned or a
    /// Base row fails to decode.
    pub fn abundance_report_intelligence(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxAbundanceReport> {
        self.backend
            .abundance_report_intelligence(panel_version, max_records)
    }

    /// Measures grounded bits per lens about one outcome anchor over a panel,
    /// persists each estimate to the native Assay CF, and reads it back.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the corpus cannot be read, the
    /// KSG estimator rejects the samples, or the Assay write/readback fails.
    pub fn assay_bits_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxAssayParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxBitsReport> {
        self.backend.assay_bits_intelligence(params)
    }

    /// Tests panel sufficiency `I(panel;anchor) >= H(anchor)`, routes deficits to
    /// logged propose-lens suggestions, persists the panel/outcome-entropy Assay
    /// rows, and reads the Assay CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the corpus cannot be read, the
    /// KSG estimator rejects the joint samples, or the Assay write/readback fails.
    pub fn assay_sufficiency_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxAssayParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxSufficiencyReport> {
        self.backend.assay_sufficiency_intelligence(params)
    }

    /// Measures pairwise lens redundancy and the panel effective rank, persists
    /// redundant pairs to the native Assay CF, and reads it back.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the corpus cannot be read, the
    /// NMI/effective-rank math fails closed, or the Assay write/readback fails.
    pub fn assay_redundancy_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxAssayParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxRedundancyReport> {
        self.backend.assay_redundancy_intelligence(params)
    }

    /// Measures pairwise lens synergy about one outcome anchor — the bits a lens
    /// pair carries beyond its better half — persists the synergistic pairs to
    /// the native Assay CF as `PairGain` rows, and reads the CF back.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when no record carries the requested
    /// anchor, the corpus cannot be read, the KSG estimator rejects the samples,
    /// or the Assay write/readback fails.
    pub fn assay_synergy_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxAssayParams,
    ) -> StorageResult<SynapseSynergyReport> {
        self.backend.assay_synergy_intelligence(params)
    }

    /// Runs the ensemble capability card over a panel: per-lens marginal value,
    /// the PID triple, the A37 associational-diversity gate, and a
    /// keep/park/retire verdict per lens (#1668's admission gate).
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when no record carries the requested
    /// anchor, the corpus cannot be read, or the Assay write/readback fails —
    /// and the #1958 structural anchor-leakage refusal when a lens on the panel
    /// reads a record field that determines the anchor.
    pub fn assay_ensemble_card_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxEnsembleCardReport> {
        self.backend
            .assay_ensemble_card_intelligence(params, min_gate_lenses)
    }

    /// Measures directed transfer entropy (KSG, lag sweep) between two activity
    /// streams over a panel and persists the dominant directed edge to the native
    /// Graph CF.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when a stream is empty, the Base CF
    /// cannot be scanned, or the Graph write/readback fails.
    pub fn temporal_causality_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxTemporalParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxCausalityReport> {
        self.backend.temporal_causality_intelligence(params)
    }

    /// Runs the Lomb-Scargle periodogram (with permutation false-alarm
    /// probability) and a slotted-autocorrelation cross-check over a panel's
    /// occurrence series and persists the result to the native `TemporalXTerm` CF.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the series is too short, the
    /// estimator fails closed, or the CF write/readback fails.
    pub fn temporal_periodicity_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxTemporalParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPeriodicityReport> {
        self.backend.temporal_periodicity_intelligence(params)
    }

    /// Detects recurrence-rate change (CUSUM) and distribution drift (MMD) over a
    /// panel's occurrence series and persists the result to the native
    /// `TemporalXTerm` CF.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the gap series is too short, an
    /// estimator fails closed, or the CF write/readback fails.
    pub fn temporal_drift_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxTemporalParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxDriftReport> {
        self.backend.temporal_drift_intelligence(params)
    }

    /// Fits the Gamma-renewal inter-event overdue hazard over a panel's
    /// occurrence series and persists the result to the native `TemporalXTerm` CF.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the occurrence series is too
    /// short, the estimator fails closed, or the CF write/readback fails.
    pub fn temporal_hazard_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxTemporalParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxHazardReport> {
        self.backend.temporal_hazard_intelligence(params)
    }

    /// The cold grounding-kernel content-slot contract declared for one panel
    /// version, or `None` when the panel declares no kernel lane.
    ///
    /// Single-sourced from the same table the cold kernel scheduler targets, so
    /// a caller that composes intelligence off a panel's kernel cannot drift
    /// onto a slot the scheduler no longer rebuilds. A panel bump that retires a
    /// lane makes this return `None` instead of silently answering on a stale
    /// slot id (#2046).
    #[must_use]
    pub fn kernel_content_slot_for_panel(panel_version: u32) -> Option<u16> {
        constellations::SYN_KERNEL_MAINTENANCE_TARGETS
            .iter()
            .find(|(declared_version, _)| *declared_version == panel_version)
            .map(|(_, content_slot)| *content_slot)
    }

    /// Builds the per-domain grounding kernel for one panel, enforces the recall
    /// gate (an ungrounded kernel is a structured error), and persists the kernel
    /// with its corpus fingerprint to the native Kernel CF.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the panel has too few embedded or
    /// no anchored concepts, the substrate kernel/recall math fails closed, the
    /// recall gate is not met, or the Kernel CF write/readback fails.
    pub fn build_domain_kernel_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxKernelParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxKernelReport> {
        self.backend.build_domain_kernel_intelligence(params)
    }

    /// Answers a grounded query through the domain kernel, returning the evidence
    /// path with hop scores, or a structured refusal that names the grounding gap
    /// (ungrounded kernel, unembedded query record, or no anchored path).
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the kernel is ungrounded, the
    /// query record is missing/unembedded, no grounded path exists, or the
    /// substrate math fails closed.
    pub fn kernel_answer_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxKernelParams,
        query_cx_id: &str,
        max_hops: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxKernelAnswerReport> {
        self.backend
            .kernel_answer_intelligence(params, query_cx_id, max_hops)
    }

    /// Reports the grounding gaps for one panel (domain): per-anchor-kind and
    /// per-lens grounded coverage, the largest ungrounded regions, and the
    /// domain's provisional verdict. Read-only physical `Base` CF readback.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the `Base` CF cannot be scanned
    /// or a constellation row fails to decode.
    pub fn grounding_gap_intelligence(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> StorageResult<synapse_calyx::SynapseCalyxGroundingGapReport> {
        self.backend
            .grounding_gap_intelligence(panel_version, max_records)
    }

    /// Scans a panel for cross-lens blind spots (records where one lens is
    /// confident a record is close to a neighbor while a second lens disagrees),
    /// calibrated for a bounded false-positive rate. Read-only.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the corpus cannot be read, the
    /// math backend is unavailable, or the calibrated detector fails closed.
    pub fn blind_spot_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxBlindSpotParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxBlindSpotReport> {
        self.backend.blind_spot_intelligence(params)
    }

    /// Measures per-lens MMD distribution drift between a reference and a recent
    /// window, persists every finding to the native `Reactive` CF, and reads it
    /// back.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the corpus cannot be read, the
    /// MMD estimator hard-fails, or the `Reactive` CF write/readback fails.
    pub fn panel_drift_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxPanelDriftParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxPanelDriftReport> {
        self.backend.panel_drift_intelligence(params)
    }

    /// Rebuilds one grounding kernel per grounded outcome domain in a panel on a
    /// COLD maintenance path, persisting each Kernel artifact to the native
    /// `Kernel` CF and reading the CF back (#1675).
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the panel has no grounded outcome
    /// domain, when every discovered domain refused the recall gate, or when the
    /// `Kernel` CF write/readback fails.
    pub fn rebuild_domain_kernels_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxKernelRebuildParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxKernelRebuildReport> {
        self.backend.rebuild_domain_kernels_intelligence(params)
    }

    /// Reports one persisted domain kernel's health by READING its Kernel
    /// artifact — recall and groundedness exactly as persisted, never re-measured.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when no kernel has been persisted for
    /// the domain or when the artifact is missing/stale/undecodable.
    pub fn domain_kernel_health_intelligence(
        &self,
        panel_version: u32,
        content_slot: u16,
        anchor_kind: Option<&str>,
    ) -> StorageResult<synapse_calyx::SynapseCalyxKernelHealthReport> {
        self.backend
            .domain_kernel_health_intelligence(panel_version, content_slot, anchor_kind)
    }

    /// Calibrates the Ward guard profile from the vault's adjudicated corpus and
    /// persists it to the native `Guard` CF for the guarded-search consumer
    /// (#1677). Fails closed rather than calibrate on a fabricated corpus.
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when the adjudicated bad-case corpus is
    /// absent, too small, or too small to certify the requested target FAR, when
    /// a slot is not dense/active in the published panel, or when the `Guard` CF
    /// write/readback fails.
    pub fn guard_calibrate_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxGuardCalibrateParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxGuardCalibrateReport> {
        self.backend.guard_calibrate_intelligence(params)
    }

    /// Verifies one record against the persisted Ward guard profile and returns
    /// the full per-slot `GuardVerdict` decomposition (#1677).
    ///
    /// # Errors
    ///
    /// Returns a structured storage error when no calibrated profile is
    /// persisted, when it was calibrated for another panel, or when the record or
    /// a trusted exemplar is missing.
    pub fn guard_verify_intelligence(
        &self,
        params: &synapse_calyx::SynapseCalyxGuardVerifyParams,
    ) -> StorageResult<synapse_calyx::SynapseCalyxGuardVerifyReport> {
        self.backend.guard_verify_intelligence(params)
    }

    /// Flushes and explicitly closes the sole process-local Calyx vault.
    ///
    /// # Errors
    ///
    /// Fails closed while any cloned physical-operation handle remains active,
    /// or when flush, PID-sidecar removal, unlock, or re-lock proof fails.
    pub fn close_calyx_vault(
        &self,
        reason: &'static str,
    ) -> StorageResult<synapse_calyx::SynapseCalyxVaultCloseReadback> {
        self.backend.close_calyx_vault(reason)
    }

    /// Atomically publishes structural positions computed from one coherent
    /// source snapshot into Graph, Registry, Base, Slot, and Ledger families.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when projection, atomic publication, or readback fails.
    pub fn publish_graph_position_snapshot(
        &self,
        kind: constellations::GraphPositionKind,
        source_seq: u64,
        created_at_ms: u64,
        transitions: &[(String, String, u64)],
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback> {
        self.backend
            .publish_graph_position_snapshot(kind, source_seq, created_at_ms, transitions)
    }

    /// Atomically publishes a hierarchy derived from one coherent source cut.
    ///
    /// # Errors
    ///
    /// Returns the structured backend error when projection, atomic publication, or readback fails.
    pub fn publish_path_hierarchy_snapshot(
        &self,
        source_seq: u64,
        created_at_ms: u64,
        paths: &[String],
    ) -> StorageResult<synapse_calyx::panel_lifecycle::SynapseCalyxDerivedSnapshotReadback> {
        self.backend
            .publish_path_hierarchy_snapshot(source_seq, created_at_ms, paths)
    }

    /// Retires every generation a dynamic panel owns other than the successor
    /// that just committed (#2062 ask 1).
    ///
    /// # Errors
    ///
    /// Fails closed when the successor is unowned, owned by another panel,
    /// itself retired, or older than a live generation of the same panel.
    pub fn supersede_panel_generations(
        &self,
        panel_name: &str,
        successor: u32,
    ) -> StorageResult<synapse_calyx::PanelGenerationSupersession> {
        self.backend
            .supersede_panel_generations(panel_name, successor)
    }

    /// Reads the vault-global panel generation allocator's ownership authority.
    ///
    /// # Errors
    ///
    /// Fails when the Registry-CF allocator row cannot be read or validated.
    pub fn panel_generation_allocator(
        &self,
    ) -> StorageResult<synapse_calyx::PanelGenerationAllocatorReadback> {
        self.backend.panel_generation_allocator()
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

    /// Measures and stores the native Calyx outcome constellation for one
    /// persisted outcome/audit row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the source CF has no outcome panel, row
    /// measurement fails, or native Calyx constellation persistence fails.
    #[tracing::instrument(skip_all, fields(source_cf, source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_outcome_constellation(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &serde_json::Value,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_outcome_constellation(source_cf, source_key, raw_bytes, record)
    }

    /// Measures and stores the native Calyx MCP usage constellation for one
    /// persisted `CF_KV mcp-usage/v1/` row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the source key is outside the MCP usage
    /// namespace, row measurement fails, or native Calyx constellation
    /// persistence fails.
    #[tracing::instrument(skip_all, fields(source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_mcp_usage_constellation(
        &self,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &serde_json::Value,
    ) -> StorageResult<ConstellationPutReport> {
        self.backend
            .put_mcp_usage_constellation(source_key, raw_bytes, record)
    }

    /// Atomically persists MCP usage source rows and their grounded native
    /// Calyx observation, then separately reads every physical source and
    /// anchor row back before returning.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the source rows do not contain exactly one
    /// requested MCP usage record, constellation or anchor validation fails,
    /// the atomic Calyx commit fails, or any separate physical readback differs
    /// from the committed source, observation, or anchor.
    #[tracing::instrument(skip_all, fields(source_row_count = source_rows.len(), source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_mcp_usage_grounded_publication(
        &self,
        source_rows: Vec<RawRow>,
        source_key: &[u8],
        raw_bytes: &[u8],
        record: &serde_json::Value,
        anchor: GroundingAnchor,
        ledger_payload: &serde_json::Value,
    ) -> StorageResult<McpUsageGroundedPublicationReport> {
        self.backend.put_mcp_usage_grounded_publication(
            source_rows,
            source_key,
            raw_bytes,
            record,
            anchor,
            ledger_payload,
        )
    }

    /// Writes a grounded anchor for one persisted source row and separately
    /// reads the physical Calyx `Anchors` CF to prove it landed.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the target constellation is missing,
    /// anchor validation fails, a conflicting anchor exists, ledger stamping
    /// fails, or the post-write `Anchors` readback does not contain exactly one
    /// matching anchor.
    #[tracing::instrument(skip_all, fields(source_cf, source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn put_grounding_anchor_for_source(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
        anchor: GroundingAnchor,
        ledger_payload: &serde_json::Value,
    ) -> StorageResult<CalyxAnchorWriteReport> {
        self.backend.put_grounding_anchor_for_source(
            source_cf,
            source_key,
            raw_bytes,
            anchor,
            ledger_payload,
        )
    }

    /// Grounds one already-measured transcript row with its declared tool-call
    /// outcome, or reports that the row carries no adjudicated outcome (#1926).
    ///
    /// Returns `Ok(None)` for every row that is not an observed `tool_result`.
    /// That is a real answer, not a skip: those rows have nothing to adjudicate.
    ///
    /// The caller must have published the row's constellation at the active
    /// panel version first. Calyx refuses an anchor whose constellation is
    /// absent (`stale_derived: constellation missing at snapshot`), so an
    /// un-measured row fails loudly here rather than accumulating a dangling
    /// anchor that a grounding readback would never see.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the row's outcome is not adjudicable or the
    /// anchor write/readback fails.
    #[tracing::instrument(skip_all, fields(line_no = record.line_no, backend = self.backend_name()))]
    pub fn put_agent_transcript_outcome_anchor(
        &self,
        source_key: &[u8],
        source_value: &[u8],
        record: &synapse_core::AgentTranscriptRecord,
    ) -> StorageResult<Option<CalyxAnchorWriteReport>> {
        let Some(anchor) = constellations::agent_transcript_outcome_anchor(record) else {
            return Ok(None);
        };
        let payload = constellations::grounding_anchor_ledger_payload(
            cf::CF_AGENT_TRANSCRIPTS,
            source_key,
            source_value,
            &anchor,
        );
        self.put_grounding_anchor_for_source(
            cf::CF_AGENT_TRANSCRIPTS,
            source_key,
            source_value,
            anchor,
            &payload,
        )
        .map(Some)
    }

    /// Writes grounded Calyx anchors for many already-persisted source rows in
    /// one durable batch, then reads every physical `Anchors` CF row back.
    ///
    /// # Errors
    ///
    /// Returns a storage error when any source row cannot map to a Calyx
    /// constellation, any anchor fails validation, the batch ledger commit
    /// fails, or any post-commit anchor readback is missing or conflicting.
    #[tracing::instrument(skip_all, fields(source_count = sources.len(), backend = self.backend_name()))]
    pub fn put_grounding_anchors_for_sources(
        &self,
        sources: Vec<GroundingAnchorSource>,
        ledger_payload: &serde_json::Value,
    ) -> StorageResult<CalyxAnchorBatchWriteReport> {
        self.backend
            .put_grounding_anchors_for_sources(sources, ledger_payload)
    }

    /// Reads decoded physical Calyx `Anchors` rows for one persisted source row.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the source CF cannot receive anchors or the
    /// physical `Anchors` CF cannot be decoded.
    #[tracing::instrument(skip_all, fields(source_cf, source_key_len = source_key.len(), backend = self.backend_name()))]
    pub fn calyx_anchor_scan_for_source(
        &self,
        source_cf: &'static str,
        source_key: &[u8],
        raw_bytes: &[u8],
    ) -> StorageResult<CalyxAnchorScanReport> {
        self.backend
            .calyx_anchor_scan_for_source(source_cf, source_key, raw_bytes)
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

    /// Reads one candidate-bounded physical page from a logical column
    /// family's Calyx namespace without materializing the whole namespace.
    ///
    /// `after_physical` is the opaque exclusive cursor returned by the prior
    /// page's [`PhysicalScanPage::resume_after_physical`]. The backend validates
    /// that it belongs to `cf_name` and advances strictly. The returned rows
    /// contain decoded logical Synapse keys/payloads; cursor ordering remains
    /// physical and must not be used to infer logical-key ordering.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the CF/cursor is invalid, the requested
    /// page exceeds Calyx's hard candidate ceiling, the backend violates its
    /// cursor/budget contract, or any physical key/value envelope is corrupt.
    #[tracing::instrument(skip_all, fields(cf_name, after_physical_len = after_physical.map_or(0, <[u8]>::len), max_rows, backend = self.backend_name()))]
    pub fn scan_cf_physical_page(
        &self,
        cf_name: &str,
        after_physical: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage> {
        self.backend
            .scan_cf_physical_page(cf_name, after_physical, max_rows)
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

    /// Scans one candidate-bounded page in `[start_key, end_key)` for one
    /// explicitly fixed user-key width.
    ///
    /// This is distinct from [`Self::scan_cf_range`]: it permits a
    /// variable-width logical column family only when both bounds name the
    /// same exact width partition. Calyx physically prefixes Synapse user keys
    /// with their length, so this contract gives the backend an honest
    /// physical range without implying that keys of other widths were
    /// searched. `after_key` is an exclusive cursor. The returned
    /// [`FixedWidthScanPage::resume_after`] includes tombstoned/expired keys,
    /// so callers can make progress even when `rows` is empty.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the bounds differ in width, are reversed,
    /// exceed Calyx's physical key-width/page-row ceilings, conflict with a
    /// schema-fixed column-family key width, or cannot be read.
    #[tracing::instrument(skip_all, fields(cf_name, start_key_len = start_key.len(), end_key_len = end_key.len(), after_key_len = after_key.map_or(0, <[u8]>::len), max_rows, backend = self.backend_name()))]
    pub fn scan_cf_fixed_width_range_page(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        after_key: Option<&[u8]>,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage> {
        self.backend
            .scan_cf_fixed_width_range_page(cf_name, start_key, end_key, after_key, max_rows)
    }

    /// Pins one bounded coherent enumeration of an entire logical column
    /// family. Continue only with [`Self::scan_cf_physical_page_coherent`] and
    /// release with [`Self::release_coherent_scan`].
    ///
    /// # Errors
    ///
    /// Returns a structured error for an unknown CF or a zero/unbounded lease.
    pub fn pin_cf_physical_scan(
        &self,
        cf_name: &str,
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease> {
        self.backend.pin_cf_physical_scan(cf_name, max_age_ms)
    }

    /// Pins one bounded coherent fixed-width range enumeration.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the CF/range is invalid or the lease
    /// lifetime exceeds [`COHERENT_SCAN_MAX_AGE_MS`].
    pub fn pin_cf_fixed_width_range_scan(
        &self,
        cf_name: &str,
        start_key: &[u8],
        end_key: &[u8],
        max_age_ms: u64,
    ) -> StorageResult<CoherentScanLease> {
        self.backend
            .pin_cf_fixed_width_range_scan(cf_name, start_key, end_key, max_age_ms)
    }

    /// Reads the next page from a pinned whole-CF enumeration. The lease owns
    /// its exclusive cursor and rejects continuation after completion/release.
    ///
    /// # Errors
    ///
    /// Returns a structured error on expiry, scope mismatch, invalid page
    /// size, cursor drift, or unavailable historical state.
    pub fn scan_cf_physical_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<PhysicalScanPage> {
        self.backend.scan_cf_physical_page_coherent(lease, max_rows)
    }

    /// Reads the next page from a pinned fixed-width range enumeration.
    ///
    /// # Errors
    ///
    /// Returns a structured error on expiry, scope mismatch, invalid page
    /// size, cursor drift, or unavailable historical state.
    pub fn scan_cf_fixed_width_range_page_coherent(
        &self,
        lease: &mut CoherentScanLease,
        max_rows: usize,
    ) -> StorageResult<FixedWidthScanPage> {
        self.backend
            .scan_cf_fixed_width_range_page_coherent(lease, max_rows)
    }

    /// Releases a bounded coherent scan lease exactly once.
    ///
    /// # Errors
    ///
    /// Returns a structured error if the handle was already released or the
    /// Calyx vault lifecycle is unavailable.
    pub fn release_coherent_scan(&self, lease: &mut CoherentScanLease) -> StorageResult<bool> {
        self.backend.release_coherent_scan(lease)
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
