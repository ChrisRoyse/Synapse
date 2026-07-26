//! Aster `VaultStore` implementation over the PH08 MVCC CF table.

mod anchor_codec;
mod anchor_compact;
mod anchor_merge;
mod backup;
mod batch_ingest;
pub(crate) mod cf_codec;
mod commit;
mod compaction_bridge;
pub mod context;
mod cursor;
mod dedup_commit;
mod durable;
pub mod encode;
mod gc_bridge;
pub mod grant;
mod grounded_observation;
mod htap;
mod ingest_precondition;
mod input_pointer;
mod key;
pub mod keyspace;
mod layer_commit;
mod ledger_anchor_batch;
mod ledger_append;
mod ledger_hook;
mod open;
mod orphan_slot_gc;
mod prepared;
pub mod quota;
mod retention_horizon;
mod router_bridge;
mod scan;
mod seq_readback;
mod slot_backfill;
mod slot_column;
mod snapshot_lease;
mod store;
mod temporal_metadata;
mod temporal_xterm;
use crate::cf::{CfRouter, ColumnFamily, KeyRange};
use crate::dedup::DedupPolicy;
use crate::mvcc::{CfRead, Freshness, ReadBarrier, Snapshot, VersionedCfStore, is_tombstone_value};
use crate::resource::{ResourceStatus, VramBudgetStatus, collect_resource_status};
use crate::timetravel::RetentionHorizon;
use crate::vault::durable::DurableVault;
use crate::vault::ledger_hook::AsterLedgerHook;
use crate::wal::{TornTail, WalRecycleReport};
use calyx_core::{CalyxError, Clock, Constellation, CxId, Result, Seq, SystemClock, VaultId};
use calyx_ledger::LedgerHeadAnchor;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

pub use anchor_compact::{AnchorCompactionConflict, AnchorCompactionReport};
pub use backup::{
    AsterBackupFile, AsterBackupReport, CALYX_ASTER_BACKUP_IO, CALYX_ASTER_BACKUP_NOT_DURABLE,
    CALYX_ASTER_BACKUP_TARGET_INVALID, EXCLUDED_RUNTIME_FILES, REGENERABLE_DIRS,
};
pub use commit::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED;
pub(crate) use compaction_bridge::LIVE_COMPACTION_TRIGGER_FILES;
pub use compaction_bridge::VaultCompactionScheduler;
pub use grant::{AuditEvent, GrantEntry, GrantStore};
pub use grounded_observation::GroundedObservationCommit;
pub use htap::HtapDualRead;
pub use ingest_precondition::{
    CALYX_INGEST_PRECONDITION_FAILED, CALYX_INGEST_PRECONDITION_INVALID, IngestPrecondition,
    IngestPreconditionClaim, IngestPreconditionContext, IngestVaultState,
};
pub use input_pointer::{CALYX_INPUT_POINTER_IDENTITY_MISMATCH, InputPointerBackfill};
pub use key::{CALYX_DECRYPTION_FAILED, CALYX_ENCRYPTION_FAILED, CALYX_VAULT_KEY_MISSING};
pub use keyspace::{
    CALYX_VAULT_KEYSPACE_MISMATCH, KeyspaceGuard, VaultWriteLock, VaultWriteLockGuard, vault_prefix,
};
pub use layer_commit::CfLedgerEntry;
pub use ledger_anchor_batch::MultiCxAnchorBatchOutcome;
pub use ledger_append::{AsterLedgerChainVerification, AsterProvenanceReproduction};
pub use orphan_slot_gc::{
    AsterOrphanSlotCfRetirement, AsterOrphanSlotCfSkip, AsterOrphanSlotGcReport,
};
pub use quota::{CALYX_QUOTA_EXCEEDED, QuotaConfig, QuotaGuard};
pub use slot_column::{
    SlotColumnManifest, SlotColumnMaterialization, SlotColumnReadback, SlotColumnRow,
    read_materialized_slot_column,
};
pub use store::{PutDisposition, PutOutcome};
pub use temporal_metadata::{
    CALYX_TEMPORAL_METADATA_MIGRATION_MISMATCH, TemporalMetadataMigration,
};
pub use {
    context::VaultContext,
    durable::{RecoveryProgressHook, VaultOptions},
};

const DEFAULT_LEASE_MS: u64 = 5_000;

/// Single-vault Aster store with content-addressed ingest semantics.
#[derive(Debug)]
pub struct AsterVault<C = SystemClock> {
    vault_id: VaultId,
    vault_salt: Vec<u8>,
    clock: C,
    rows: VersionedCfStore,
    durable: Option<DurableVault>,
    dedup_policy: DedupPolicy,
    retention_horizon: Mutex<RetentionHorizon>,
    ledger_hook: Option<AsterLedgerHook>,
    read_only: bool,
    commit_lock: Mutex<()>,
    /// Threads currently queued for the durable commit lock (issue #1806).
    ///
    /// Maintenance drains use this to decide whether to hand the lock off to a
    /// waiting writer between bounded units instead of immediately re-taking
    /// it, and the slow-hold telemetry reports it so a stall can be attributed
    /// to contention rather than to a single slow operation.
    commit_lock_waiters: AtomicUsize,
    recurrence_write_lock: Mutex<()>,
    ledger_state_reconciliation_required: AtomicBool,
    /// Exact sequence of the most recent commit that returned an error after
    /// crossing its irreversible publication boundary. This is consumed while
    /// the durable commit lock is still held by APIs that expose typed
    /// committed-outcome metadata; zero means no such outcome is pending.
    post_commit_error_seq: AtomicU64,
    recovery_report: VaultRecoveryReport,
    residency: Option<crate::residency::Residency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRecoveryReport {
    pub last_recovered_seq: Seq,
    pub torn_tail: Option<TornTail>,
}

/// Bounded commit-lock acquisitions one paced checkpoint drain may use before
/// it fails closed (issue #1806).
///
/// At the 256-batch / 250 ms per-hold bound this covers a backlog of up to
/// ~1M staged group commits. Exceeding it means commits are arriving faster
/// than durable checkpoints can be materialized, which is a real capacity
/// fault: it is reported, never papered over by reverting to one unbounded
/// hold.
const PACED_CHECKPOINT_MAX_HOLDS: usize = 4_096;

/// Pause used to hand the durable commit lock to a queued writer between
/// bounded maintenance units. `std::sync::Mutex` is not fair, so without an
/// explicit pause a draining thread re-acquires immediately and the pacing
/// delivers no write progress.
const COMMIT_LOCK_HANDOFF_PAUSE: Duration = Duration::from_millis(2);

/// Stable error code for an invalid revision-guarded CF mutation.
pub const CALYX_ASTER_CONDITIONAL_WRITE_INVALID: &str = "CALYX_ASTER_CONDITIONAL_WRITE_INVALID";
/// Stable error code for attempts to bypass the append-only Ledger API and its
/// persistent-hook reconciliation contract.
pub const CALYX_ASTER_LEDGER_RAW_WRITE_FORBIDDEN: &str = "CALYX_ASTER_LEDGER_RAW_WRITE_FORBIDDEN";

/// One physical CF revision precondition.
///
/// `expected_revision` is SHA-256 over the exact latest plaintext value, or
/// `None` when the key must be physically absent. Guards are evaluated in
/// input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfRevisionGuard {
    pub cf: ColumnFamily,
    pub key: Vec<u8>,
    pub expected_revision: Option<[u8; 32]>,
}

impl CfRevisionGuard {
    #[must_use]
    pub fn new(
        cf: ColumnFamily,
        key: impl Into<Vec<u8>>,
        expected_revision: Option<[u8; 32]>,
    ) -> Self {
        Self {
            cf,
            key: key.into(),
            expected_revision,
        }
    }
}

/// The first ordered guard whose expected revision did not match reality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalCfWriteConflict {
    pub guard_index: usize,
    pub cf: ColumnFamily,
    pub key: Vec<u8>,
    pub expected_revision: Option<[u8; 32]>,
    pub actual_revision: Option<[u8; 32]>,
}

/// Result of one durable multi-key revision-guarded CF batch.
///
/// `actual_revisions` always contains one pre-commit revision per guard in
/// guard-input order. On success, `committed_revisions` has the same length;
/// a deleted guard is represented by `None`. On conflict, no mutation occurs,
/// `committed_revisions` is empty, and `conflict` identifies the first
/// mismatching guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiConditionalCfWriteOutcome {
    pub applied: bool,
    pub seq: Seq,
    pub actual_revisions: Vec<Option<[u8; 32]>>,
    pub committed_revisions: Vec<Option<[u8; 32]>>,
    pub conflict: Option<ConditionalCfWriteConflict>,
}

/// Error from a revision-guarded batch, including an exact applied sequence
/// when the operation crossed the commit boundary before failing.
///
/// `committed_seq = Some(_)` is a fail-stop outcome: callers must reconcile the
/// identified commit from physical truth and must not blindly retry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalCfWriteError {
    pub source: CalyxError,
    pub committed_seq: Option<Seq>,
}

impl std::fmt::Display for ConditionalCfWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.committed_seq {
            Some(seq) => write!(formatter, "{}; committed_seq={seq}", self.source),
            None => self.source.fmt(formatter),
        }
    }
}

impl std::error::Error for ConditionalCfWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Compatibility result for one durable revision-guarded CF batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionalCfWriteOutcome {
    pub applied: bool,
    pub seq: Seq,
    pub previous_revision: Option<[u8; 32]>,
    pub committed_revision: Option<[u8; 32]>,
}

fn value_revision(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn invalid_conditional_write(message: String) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_CONDITIONAL_WRITE_INVALID,
        message,
        remediation: "supply a non-empty unique guard set with non-empty keys and at most one mutation for each guarded CF/key",
    }
}

fn conditional_write_error(source: CalyxError) -> ConditionalCfWriteError {
    ConditionalCfWriteError {
        source,
        committed_seq: None,
    }
}

fn nonzero_seq(seq: Seq) -> Option<Seq> {
    (seq != 0).then_some(seq)
}

fn raw_ledger_write_forbidden(operation: &str, detail: String) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_LEDGER_RAW_WRITE_FORBIDDEN,
        message: format!("{operation}: {detail}"),
        remediation: "write Ledger entries through append_ledger_entry, append_external_ledger_row, or a ledger-stamped vault operation so append-only validation, sidecars, and the persistent hook advance together",
    }
}

fn reject_raw_ledger_rows(operation: &str, rows: &[encode::WriteRow]) -> Result<()> {
    if let Some((row_index, row)) = rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.cf == ColumnFamily::Ledger)
    {
        return Err(raw_ledger_write_forbidden(
            operation,
            format!(
                "raw Ledger mutation is reserved: row_index={row_index} key_len={} value_len={}",
                row.key.len(),
                row.value.len()
            ),
        ));
    }
    Ok(())
}

fn reject_raw_ledger_guards(operation: &str, guards: &[CfRevisionGuard]) -> Result<()> {
    if let Some((guard_index, guard)) = guards
        .iter()
        .enumerate()
        .find(|(_, guard)| guard.cf == ColumnFamily::Ledger)
    {
        return Err(raw_ledger_write_forbidden(
            operation,
            format!(
                "raw Ledger revision guard is reserved: guard_index={guard_index} key_len={}",
                guard.key.len()
            ),
        ));
    }
    Ok(())
}

fn guarded_row_indices(
    guards: &[CfRevisionGuard],
    rows: &[encode::WriteRow],
) -> Result<Vec<Option<usize>>> {
    if guards.is_empty() {
        return Err(invalid_conditional_write(
            "conditional CF write requires at least one revision guard".to_owned(),
        ));
    }

    let mut seen_guards = BTreeSet::new();
    for (guard_index, guard) in guards.iter().enumerate() {
        if guard.key.is_empty() {
            return Err(invalid_conditional_write(format!(
                "conditional CF write guard key must be non-empty: guard_index={guard_index} guard_cf={:?}",
                guard.cf
            )));
        }
        if !seen_guards.insert((guard.cf, guard.key.as_slice())) {
            return Err(invalid_conditional_write(format!(
                "conditional CF write guard keys must be unique: guard_index={guard_index} guard_cf={:?} guard_key_len={}",
                guard.cf,
                guard.key.len()
            )));
        }
    }

    let mut row_occurrences = BTreeMap::<(ColumnFamily, &[u8]), (usize, usize)>::new();
    for (row_index, row) in rows.iter().enumerate() {
        row_occurrences
            .entry((row.cf, row.key.as_slice()))
            .and_modify(|(_, count)| *count += 1)
            .or_insert((row_index, 1));
    }

    guards
        .iter()
        .enumerate()
        .map(|(guard_index, guard)| {
            let (row_index, matching_rows) = row_occurrences
                .get(&(guard.cf, guard.key.as_slice()))
                .copied()
                .unwrap_or((0, 0));
            if matching_rows > 1 {
                return Err(invalid_conditional_write(format!(
                    "conditional CF write permits at most one mutation for every guard: guard_index={guard_index} guard_cf={:?} guard_key_len={} matching_rows={matching_rows} total_rows={}",
                    guard.cf,
                    guard.key.len(),
                    rows.len()
                )));
            }
            Ok((matching_rows == 1).then_some(row_index))
        })
        .collect()
}

impl AsterVault<SystemClock> {
    /// Creates a vault using the system clock.
    pub fn new(vault_id: VaultId, vault_salt: impl Into<Vec<u8>>) -> Self {
        Self::with_clock(vault_id, vault_salt, SystemClock)
    }

    pub fn new_durable(
        vault_dir: impl AsRef<Path>,
        vault_id: VaultId,
        vault_salt: impl Into<Vec<u8>>,
        options: VaultOptions,
    ) -> Result<Self> {
        Self::open(vault_dir, vault_id, vault_salt, options)
    }

    pub fn open(
        vault_dir: impl AsRef<Path>,
        vault_id: VaultId,
        vault_salt: impl Into<Vec<u8>>,
        options: VaultOptions,
    ) -> Result<Self> {
        AsterVault::open_with_clock(vault_dir, vault_id, vault_salt, options, SystemClock)
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Opens a durable vault with an injected clock.
    ///
    /// Production callers use [`AsterVault::open`] with [`SystemClock`]. This
    /// constructor exists for deterministic FSV: the vault remains fully
    /// durable, but group commits stamp `time_index` rows from `clock`.
    pub fn new_durable_with_clock(
        vault_dir: impl AsRef<Path>,
        vault_id: VaultId,
        vault_salt: impl Into<Vec<u8>>,
        options: VaultOptions,
        clock: C,
    ) -> Result<Self> {
        Self::open_with_clock(vault_dir, vault_id, vault_salt, options, clock)
    }

    /// Creates a vault with an injected clock.
    pub fn with_clock(vault_id: VaultId, vault_salt: impl Into<Vec<u8>>, clock: C) -> Self {
        Self {
            vault_id,
            vault_salt: vault_salt.into(),
            clock,
            rows: VersionedCfStore::default(),
            durable: None,
            dedup_policy: DedupPolicy::default(),
            retention_horizon: Mutex::new(RetentionHorizon::default()),
            ledger_hook: None,
            read_only: false,
            commit_lock: Mutex::new(()),
            commit_lock_waiters: AtomicUsize::new(0),
            recurrence_write_lock: Mutex::new(()),
            ledger_state_reconciliation_required: AtomicBool::new(false),
            post_commit_error_seq: AtomicU64::new(0),
            recovery_report: VaultRecoveryReport {
                last_recovered_seq: 0,
                torn_tail: None,
            },
            residency: None,
        }
    }

    /// Returns the vault's data-residency pin, if one is set (PRD `30 §4`).
    pub fn residency(&self) -> Option<&crate::residency::Residency> {
        self.residency.as_ref()
    }

    pub(crate) fn ensure_writeable(&self, operation: &str) -> Result<()> {
        if !self.read_only {
            return Ok(());
        }
        Err(CalyxError {
            code: "CALYX_VAULT_READ_ONLY",
            message: format!("read-only Aster vault handle rejected {operation}"),
            remediation: "open a write-capable vault handle with read_only=false for mutating operations",
        })
    }

    /// Authorizes an external copy/export to `target` against the residency pin.
    /// With no pin set, every target is authorized. On a violation, an
    /// `EntryKind::Admin` governance entry is written to the Ledger (the audit
    /// trail) and `CALYX_RESIDENCY_VIOLATION` is returned — fail closed, never a
    /// silent off-dataset copy.
    pub fn authorize_external_copy(&self, target: &std::path::Path) -> Result<()> {
        let Some(residency) = &self.residency else {
            return Ok(());
        };
        match residency.authorize(target) {
            Ok(()) => Ok(()),
            Err(violation) => {
                // The Ledger forbids raw paths (potential secrets), so the
                // immutable audit references paths by verifiable blake3 digest;
                // the human-readable paths travel in the returned error message.
                let payload = serde_json::to_vec(&serde_json::json!({
                    "event": "residency_violation",
                    "dataset_root_hash": residency.dataset_root_digest(),
                    "attempted_target_hash": crate::residency::Residency::path_digest(target),
                    "allow_off_dataset": residency.allow_off_dataset,
                }))
                .map_err(|error| CalyxError {
                    code: "CALYX_RESIDENCY_CORRUPT",
                    message: format!("encode residency audit payload: {error}"),
                    remediation: "report this bug; residency audit payload must be serializable",
                })?;
                self.append_ledger_entry(
                    calyx_ledger::EntryKind::Admin,
                    calyx_ledger::SubjectId::Guard(residency.audit_subject()),
                    payload,
                    calyx_ledger::ActorId::System,
                )?;
                Err(violation)
            }
        }
    }

    pub fn with_clock_and_dedup_policy(
        vault_id: VaultId,
        vault_salt: impl Into<Vec<u8>>,
        clock: C,
        dedup_policy: DedupPolicy,
    ) -> Result<Self> {
        dedup_policy.validate_manifest()?;
        let mut vault = Self::with_clock(vault_id, vault_salt, clock);
        vault.dedup_policy = dedup_policy;
        Ok(vault)
    }

    /// Computes the PRD content-addressed id for raw input bytes.
    pub fn cx_id_for_input(&self, input_bytes: &[u8], panel_version: u32) -> CxId {
        CxId::from_input(input_bytes, panel_version, &self.vault_salt)
    }

    /// Returns the latest committed vault sequence.
    pub fn latest_seq(&self) -> Seq {
        self.rows.current_seq()
    }

    /// Latest committed seq whose batch wrote derived-search-content inputs
    /// (issue #1100). Content-neutral commits (idempotency-ledger appends,
    /// time-index sentinels) advance [`Self::latest_seq`] but not this.
    pub fn derived_content_seq(&self) -> Seq {
        self.rows.derived_content_seq()
    }

    pub fn recovery_report(&self) -> &VaultRecoveryReport {
        &self.recovery_report
    }

    pub fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    pub(crate) fn clock_now(&self) -> u64 {
        self.clock.now()
    }

    pub fn dedup_policy(&self) -> &DedupPolicy {
        &self.dedup_policy
    }

    /// Reads one raw CF row from one atomic view of the latest committed state.
    pub fn read_cf_latest(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.rows.read_latest(cf, key)
    }

    /// Reads one raw CF row and its physical-value revision from one atomic
    /// latest committed view.
    ///
    /// The revision is SHA-256 over the exact plaintext CF value returned by
    /// Aster. It can be supplied to [`Self::write_cf_batch_if_revision`] as an
    /// optimistic concurrency precondition.
    pub fn read_cf_latest_revisioned(
        &self,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<(Vec<u8>, [u8; 32])>> {
        self.rows.read_latest(cf, key).map(|value| {
            value.map(|value| {
                let revision = value_revision(&value);
                (value, revision)
            })
        })
    }

    /// Reads raw CF rows from one atomic view of the latest committed state.
    pub fn read_cf_batch_latest(
        &self,
        reads: &[crate::mvcc::CfRead],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.rows.read_batch_latest(reads)
    }

    /// Reads one raw CF row at `snapshot`.
    pub fn read_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let snapshot = self.snapshot_handle(snapshot);
        self.rows.read_at(snapshot.snapshot(), cf, key, &self.clock)
    }

    /// Reads one raw CF row using an already-pinned snapshot lease.
    pub fn read_cf_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.rows.read_at(snapshot, cf, key, &self.clock)
    }

    /// Writes raw CF rows through the same WAL/MVCC commit path as vault puts.
    pub fn write_cf_batch(
        &self,
        rows: impl IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
    ) -> Result<Seq> {
        let rows = rows
            .into_iter()
            .map(|(cf, key, value)| encode::WriteRow { cf, key, value })
            .collect::<Vec<_>>();
        reject_raw_ledger_rows("write_cf_batch", &rows)?;
        if rows.is_empty() {
            return Ok(self.latest_seq());
        }
        self.commit_rows(&rows)
    }

    /// Atomically compares all latest CF value revisions and commits one batch.
    ///
    /// The guard list and every guard key must be non-empty, guards must be
    /// unique by `(cf, key)`, and a guarded key may occur at most once in
    /// `rows`. A guard with no matching row is a read-only precondition: its
    /// observed revision is returned unchanged after the commit. Validation
    /// happens before lock acquisition. Revision comparison, the single
    /// WAL/MVCC commit, and outcome construction share the same process and
    /// cross-process commit boundary.
    ///
    /// A conflict returns the first mismatching guard in input order plus the
    /// actual revisions of every guard. It does not append to the WAL, advance
    /// MVCC sequence state, or mutate any row.
    ///
    /// # Errors
    ///
    /// Returns [`CALYX_ASTER_CONDITIONAL_WRITE_INVALID`] for malformed guards
    /// or mutations. Other structured errors report read barriers, read-only
    /// handles, admission, WAL, MVCC, or durability failures.
    pub fn write_cf_batch_if_revisions(
        &self,
        guards: impl IntoIterator<Item = CfRevisionGuard>,
        rows: impl IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
    ) -> std::result::Result<MultiConditionalCfWriteOutcome, ConditionalCfWriteError> {
        let guards = guards.into_iter().collect::<Vec<_>>();
        let rows = rows
            .into_iter()
            .map(|(cf, key, value)| encode::WriteRow { cf, key, value })
            .collect::<Vec<_>>();
        reject_raw_ledger_guards("write_cf_batch_if_revisions", &guards)
            .map_err(conditional_write_error)?;
        reject_raw_ledger_rows("write_cf_batch_if_revisions", &rows)
            .map_err(conditional_write_error)?;
        let guard_row_indices =
            guarded_row_indices(&guards, &rows).map_err(conditional_write_error)?;
        self.ensure_writeable("revision-guarded CF batch")
            .map_err(conditional_write_error)?;

        let mut committed_seq_on_error = None;
        let outcome = self.with_durable_commit_lock(|| {
            let reads = guards
                .iter()
                .map(|guard| CfRead::new(guard.cf, guard.key.clone()))
                .collect::<Vec<_>>();
            let actual_revisions = self
                .rows
                .read_batch_latest(&reads)?
                .iter()
                .map(|value| value.as_deref().map(value_revision))
                .collect::<Vec<_>>();

            if let Some(guard_index) = guards
                .iter()
                .zip(&actual_revisions)
                .position(|(guard, actual)| guard.expected_revision != *actual)
            {
                let guard = &guards[guard_index];
                return Ok(MultiConditionalCfWriteOutcome {
                    applied: false,
                    seq: self.latest_seq(),
                    conflict: Some(ConditionalCfWriteConflict {
                        guard_index,
                        cf: guard.cf,
                        key: guard.key.clone(),
                        expected_revision: guard.expected_revision,
                        actual_revision: actual_revisions[guard_index],
                    }),
                    actual_revisions,
                    committed_revisions: Vec::new(),
                });
            }

            // Clear any prior operation's marker while this commit boundary is
            // exclusively held. `commit_prepared_rows` sets it only after an
            // irreversible commit has occurred and a later step fails.
            self.post_commit_error_seq.store(0, Ordering::Release);
            let seq = match self.commit_rows_locked(&rows) {
                Ok(seq) => seq,
                Err(error) => {
                    committed_seq_on_error =
                        nonzero_seq(self.post_commit_error_seq.swap(0, Ordering::AcqRel));
                    return Err(error);
                }
            };
            let committed_revisions = guard_row_indices
                .iter()
                .enumerate()
                .map(|(guard_index, row_index)| {
                    row_index.map_or(actual_revisions[guard_index], |row_index| {
                        let value = &rows[row_index].value;
                        (!is_tombstone_value(value)).then(|| value_revision(value))
                    })
                })
                .collect();
            Ok(MultiConditionalCfWriteOutcome {
                applied: true,
                seq,
                actual_revisions,
                committed_revisions,
                conflict: None,
            })
        });
        outcome.map_err(|source| ConditionalCfWriteError {
            source,
            committed_seq: committed_seq_on_error,
        })
    }

    /// Atomically compares one latest CF value revision and commits a batch.
    ///
    /// This compatibility wrapper delegates to
    /// [`Self::write_cf_batch_if_revisions`].
    pub fn write_cf_batch_if_revision(
        &self,
        guard_cf: ColumnFamily,
        guard_key: &[u8],
        expected_revision: Option<[u8; 32]>,
        rows: impl IntoIterator<Item = (ColumnFamily, Vec<u8>, Vec<u8>)>,
    ) -> Result<ConditionalCfWriteOutcome> {
        let outcome = self
            .write_cf_batch_if_revisions(
                [CfRevisionGuard::new(guard_cf, guard_key, expected_revision)],
                rows,
            )
            .map_err(|error| error.source)?;
        let previous_revision = outcome.actual_revisions.first().copied().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "single-key conditional write returned no actual guard revision",
            )
        })?;
        let committed_revision = if outcome.applied {
            outcome
                .committed_revisions
                .first()
                .copied()
                .ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(
                        "applied single-key conditional write returned no committed guard revision",
                    )
                })?
        } else {
            None
        };
        Ok(ConditionalCfWriteOutcome {
            applied: outcome.applied,
            seq: outcome.seq,
            previous_revision,
            committed_revision,
        })
    }

    /// Writes one raw CF row through the WAL-backed batch path.
    pub fn write_cf(&self, cf: ColumnFamily, key: Vec<u8>, value: Vec<u8>) -> Result<Seq> {
        self.write_cf_batch([(cf, key, value)])
    }

    /// Internal compatibility path for a `LedgerCfStore` operating on a vault
    /// that has no persistent in-memory Ledger hook. Public raw CF APIs reserve
    /// Ledger so a caller cannot make a configured hook stale. Semantic
    /// admission, authoritative chain validation, absence, and commit share
    /// the durable boundary.
    fn write_raw_ledger_row_without_hook(&self, seq: u64, value: &[u8]) -> Result<Seq> {
        if self.ledger_hook.is_some() {
            return Err(raw_ledger_write_forbidden(
                "write_raw_ledger_row_without_hook",
                format!(
                    "trusted no-hook path was invoked while a persistent Ledger hook is configured: key_len={} value_len={}",
                    crate::cf::ledger_key(seq).len(),
                    value.len()
                ),
            ));
        }
        self.with_durable_commit_lock(|| {
            self.validate_decoded_ledger_append_locked(
                "write_raw_ledger_row_without_hook",
                seq,
                value,
            )?;
            let key = crate::cf::ledger_key(seq);
            self.commit_rows_locked(&[encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key,
                value: value.to_vec(),
            }])
        })
    }

    /// Validates the head callback made by `LedgerAppender` after the trusted
    /// no-hook row commit. `commit_rows_locked` already advanced the durable
    /// sidecar while holding the process and cross-process commit boundary, so
    /// this callback must never perform a second, unlocked sidecar write.
    pub(super) fn validate_committed_ledger_head_anchor(
        &self,
        requested: &LedgerHeadAnchor,
    ) -> Result<()> {
        match self.with_durable_commit_lock(|| {
            self.validate_committed_ledger_head_anchor_locked(requested)
        }) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.committed_ledger_head_validation_error(requested, error)),
        }
    }

    pub(super) fn validate_committed_ledger_head_anchor_locked(
        &self,
        requested: &LedgerHeadAnchor,
    ) -> Result<()> {
        let validation = (|| -> Result<()> {
            if requested.height == 0 {
                return Err(CalyxError::ledger_chain_broken(
                    "post-append Ledger head validation received height 0",
                ));
            }
            self.validate_ledger_head_tip_row_locked(requested, "requested post-append head")?;
            let Some(durable) = &self.durable else {
                return Ok(());
            };
            let physical =
                crate::ledger_head::read_head_anchor(durable.root())?.ok_or_else(|| {
                    CalyxError::ledger_chain_broken(format!(
                        "durable Ledger head sidecar is missing after committing height {}",
                        requested.height
                    ))
                })?;
            match physical.height.cmp(&requested.height) {
                std::cmp::Ordering::Less => Err(CalyxError::ledger_chain_broken(format!(
                    "durable Ledger head sidecar regressed: requested_height={} physical_height={}",
                    requested.height, physical.height
                ))),
                std::cmp::Ordering::Equal if physical.tip_hash != requested.tip_hash => {
                    Err(CalyxError::ledger_chain_broken(format!(
                        "durable Ledger head sidecar hash mismatch at height {}: requested_tip={:02x?} physical_tip={:02x?}",
                        requested.height, requested.tip_hash, physical.tip_hash
                    )))
                }
                std::cmp::Ordering::Equal => Ok(()),
                std::cmp::Ordering::Greater => self
                    .validate_ledger_head_tip_row_locked(&physical, "newer durable head sidecar"),
            }
        })();
        validation.map_err(|error| self.committed_ledger_head_validation_error(requested, error))
    }

    fn validate_ledger_head_tip_row_locked(
        &self,
        anchor: &LedgerHeadAnchor,
        witness: &'static str,
    ) -> Result<()> {
        let row_seq = anchor.height.checked_sub(1).ok_or_else(|| {
            CalyxError::ledger_chain_broken(format!(
                "{witness} has height 0 and cannot witness a committed row"
            ))
        })?;
        let key = crate::cf::ledger_key(row_seq);
        let bytes = self
            .read_cf_at(self.latest_seq(), ColumnFamily::Ledger, &key)?
            .ok_or_else(|| {
                CalyxError::ledger_chain_broken(format!(
                    "{witness} height {} requires missing Ledger seq {row_seq}",
                    anchor.height
                ))
            })?;
        let entry = calyx_ledger::decode(&bytes)?;
        if entry.seq != row_seq || entry.entry_hash != anchor.tip_hash {
            return Err(CalyxError::ledger_chain_broken(format!(
                "{witness} tip mismatch: height={} row_seq={row_seq} encoded_seq={} witness_tip={:02x?} row_tip={:02x?}",
                anchor.height, entry.seq, anchor.tip_hash, entry.entry_hash
            )));
        }
        Ok(())
    }

    fn committed_ledger_head_validation_error(
        &self,
        requested: &LedgerHeadAnchor,
        error: CalyxError,
    ) -> CalyxError {
        self.ledger_state_reconciliation_required
            .store(true, std::sync::atomic::Ordering::Release);
        if error.code == CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED {
            return error;
        }
        tracing::error!(
            code = CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
            requested_height = requested.height,
            requested_tip = ?requested.tip_hash,
            validation_error_code = error.code,
            validation_error = %error,
            "Ledger row is committed but its post-commit head witness could not be validated"
        );
        CalyxError {
            code: CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED,
            message: format!(
                "Ledger row at committed height {} cannot complete post-commit head validation: validation=error[{}]: {}",
                requested.height, error.code, error.message
            ),
            remediation: "treat the Ledger row as committed; reconcile the physical Ledger rows and derived head sidecar before retrying the logical operation",
        }
    }

    /// Scans visible raw CF rows at `snapshot`; use `scan_cf_pages_at` for large data CFs.
    pub fn scan_cf_at(&self, snapshot: Seq, cf: ColumnFamily) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let snapshot = self.snapshot_handle(snapshot);
        self.rows.scan_cf_at(snapshot.snapshot(), cf, &self.clock)
    }

    /// Scans visible raw CF rows for a pinned lease; use `scan_cf_pages_snapshot` for large data.
    pub fn scan_cf_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.rows.scan_cf_at(snapshot, cf, &self.clock)
    }

    /// Scans visible raw CF rows from one atomic latest committed view.
    pub fn scan_cf_latest(&self, cf: ColumnFamily) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.rows.scan_cf_latest(cf)
    }

    /// Scans visible raw CF rows in a key range at `snapshot`.
    pub fn scan_cf_range_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let snapshot = self.snapshot_handle(snapshot);
        self.rows
            .scan_cf_range_at(snapshot.snapshot(), cf, range, &self.clock)
    }

    /// Scans visible raw CF rows in a key range using an already-pinned snapshot lease.
    pub fn scan_cf_range_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.rows.scan_cf_range_at(snapshot, cf, range, &self.clock)
    }

    /// Scans visible raw CF rows in a range from one atomic latest committed view.
    pub fn scan_cf_range_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.rows.scan_cf_range_latest(cf, range)
    }

    /// Reads one candidate-bounded page from an atomic latest committed view.
    pub fn scan_cf_range_page_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<crate::mvcc::LatestCfRangePage> {
        self.rows
            .scan_cf_range_page_latest(cf, range, after_key, limit)
    }

    /// Scans visible raw CF row keys in a key range at `snapshot`.
    pub fn scan_cf_range_keys_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<Vec<u8>>> {
        let snapshot = self.snapshot_handle(snapshot);
        self.rows
            .scan_cf_range_keys_at(snapshot.snapshot(), cf, range, &self.clock)
    }

    /// Scans at most `limit` visible raw CF rows in key order after `after_key`.
    pub fn scan_cf_range_page_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let snapshot = self.snapshot_handle(snapshot);
        self.rows.scan_cf_range_page_at(
            snapshot.snapshot(),
            cf,
            range,
            after_key,
            limit,
            &self.clock,
        )
    }

    /// Reads the greatest visible raw CF row in `[start, upper]` at `snapshot`.
    pub fn predecessor_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        start: &[u8],
        upper: &[u8],
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let snapshot = self.snapshot_handle(snapshot);
        self.rows
            .predecessor_cf_at(snapshot.snapshot(), cf, start, upper, &self.clock)
    }

    pub(super) fn stage_constellation_rows(
        &self,
        rows: &mut Vec<encode::WriteRow>,
        constellation: &Constellation,
    ) -> Result<()> {
        constellation.validate_schema()?;
        let prepared = prepared::PreparedConstellationEncoding::new(constellation)?;
        prepared::stage_validated_constellation_rows(rows, constellation, prepared)
    }

    pub fn flush(&self) -> Result<()> {
        self.drain_checkpoints_paced("flush")?;
        self.with_durable_commit_lock(|| {
            self.ensure_writeable("flush")?;
            self.rows.flush_all_cfs().map(|_| ())
        })
    }

    /// Materializes every staged durable checkpoint without holding the global
    /// durable commit lock across filesystem I/O (issues #1806 and #1832).
    ///
    /// Root cause this exists for: one durable group commit stages one
    /// checkpoint batch, and each batch becomes one create+fsync+rename SST
    /// per touched CF. A derived-write fanout backfill stages tens of
    /// thousands of those between 30 s checkpoint ticks, and the previous
    /// single-acquisition drain therefore held the *only* lock serializing
    /// vault writes for the entire backlog — observed at 86 s in this vault's
    /// own boot telemetry (`CALYX_ASTER_NATIVE_FANOUT_MAINTENANCE_START
    /// commit_lock_hold_us=86096312`) and at 469 s live during the #1782 FSV.
    /// Writers, including the activity-recorder write that MCP `initialize`
    /// performs, queued behind it for the whole drain.
    ///
    /// Each iteration reserves a contiguous staged prefix under a short commit
    /// boundary, then releases it for immutable SST create+fsync+rename and
    /// manifest publication. A dedicated process/cross-process checkpoint lock
    /// serializes manifest publishers and is always acquired before the short
    /// global-lock reserve, preventing both filename collisions and lock-order
    /// deadlocks. Concurrent commits serialize only on the staging mutex while
    /// the prepared prefix is validated; they never wait for manifest fsync.
    /// Any failure re-stages the exact prefix before returning.
    pub(crate) fn drain_checkpoints_paced(&self, operation: &'static str) -> Result<()> {
        if self.durable.is_none() || self.read_only {
            return Ok(());
        }
        let durable = self.durable.as_ref().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "write-capable checkpoint drain lost its durable vault handle",
            )
        })?;
        let checkpoint_lock_path = durable.checkpoint_lock_path();
        let checkpoint_lock_started = Instant::now();
        let _checkpoint_guard = crate::file_lock::FileLockGuard::acquire(&checkpoint_lock_path)?;
        let checkpoint_lock_wait_ms = checkpoint_lock_started.elapsed().as_millis();
        let started = Instant::now();
        let mut chunks = 0_usize;
        let mut batches_written = 0_usize;
        let mut rows_written = 0_usize;
        let mut sst_files_written = 0_usize;
        let mut sst_bytes_written = 0_u64;
        let mut max_reserve_elapsed_ms = 0_u128;
        let mut max_materialize_ms = 0_u128;
        let mut max_publish_elapsed_ms = 0_u128;
        loop {
            // Sync the WAL BEFORE taking the commit lock.
            //
            // `sync_wal` is `flush_sync`, which posts a Flush to the
            // group-commit batcher thread over an mpsc channel and blocks for
            // its ack — so it waits behind every WAL append already queued on
            // that thread. Doing that inside the exclusive hold parked every
            // vault write behind another thread's queue depth: measured 37.6 s
            // of hold for a drain that materialized 4 batches / 72 rows, i.e. a
            // fixed cost with no relation to the work done (#1832).
            //
            // Durability is unchanged. The #1132 invariant is that the manifest
            // may only advance past a batch once that batch's rows exist as
            // fsynced durable-batch SSTs, and that is enforced below by the
            // coalesced flush itself, not by this sync. A batch staged between
            // this sync and the flush is therefore still recoverable from its
            // SST; this sync covers the WAL tail *above* the new replay floor,
            // which is exactly the part no SST covers.
            durable.sync_wal()?;
            let reserve_started = Instant::now();
            let prepared = self.with_durable_commit_lock(|| {
                self.ensure_writeable("checkpoint")?;
                durable.prepare_pending_checkpoint(durable::CHECKPOINT_DRAIN_MAX_BATCHES)
            })?;
            max_reserve_elapsed_ms =
                max_reserve_elapsed_ms.max(reserve_started.elapsed().as_millis());
            let Some(prepared) = prepared else {
                break;
            };
            let materialize_started = Instant::now();
            let materialized = match durable.materialize_prepared_checkpoint(&prepared) {
                Ok(materialized) => materialized,
                Err(error) => {
                    tracing::error!(
                        code = "CALYX_ASTER_CHECKPOINT_MATERIALIZE_FAILED",
                        operation,
                        base_durable_seq = prepared.base_durable_seq,
                        first_seq = prepared.first_seq,
                        last_seq = prepared.last_seq,
                        batches = prepared.batch_count(),
                        rows = prepared.rows,
                        bytes = prepared.bytes,
                        elapsed_ms = materialize_started.elapsed().as_millis(),
                        error_code = error.code,
                        error = %error.message,
                        "checkpoint SST materialization failed outside the global commit lock; restoring the reserved prefix"
                    );
                    if let Err(restage) = durable.restage_prepared_checkpoint(prepared) {
                        return Err(CalyxError::aster_corrupt_shard(format!(
                            "checkpoint materialization failed with error[{}]: {}; restoring its reserved prefix also failed with error[{}]: {}",
                            error.code, error.message, restage.code, restage.message
                        )));
                    }
                    return Err(error);
                }
            };
            let materialize_ms = materialize_started.elapsed().as_millis();
            max_materialize_ms = max_materialize_ms.max(materialize_ms);
            let publish_started = Instant::now();
            let chunk = match (|| {
                self.ensure_writeable("checkpoint publication")?;
                durable.publish_prepared_checkpoint(&prepared)
            })() {
                Ok(chunk) => chunk,
                Err(error) => {
                    tracing::error!(
                        code = "CALYX_ASTER_CHECKPOINT_PUBLISH_FAILED",
                        operation,
                        base_durable_seq = prepared.base_durable_seq,
                        first_seq = prepared.first_seq,
                        last_seq = prepared.last_seq,
                        batches = prepared.batch_count(),
                        rows = prepared.rows,
                        materialize_ms,
                        error_code = error.code,
                        error = %error.message,
                        "checkpoint manifest publication failed; restoring the materialized prefix for an idempotent retry"
                    );
                    if let Err(restage) = durable.restage_prepared_checkpoint(prepared) {
                        return Err(CalyxError::aster_corrupt_shard(format!(
                            "checkpoint publication failed with error[{}]: {}; restoring its reserved prefix also failed with error[{}]: {}",
                            error.code, error.message, restage.code, restage.message
                        )));
                    }
                    return Err(error);
                }
            };
            max_publish_elapsed_ms =
                max_publish_elapsed_ms.max(publish_started.elapsed().as_millis());
            chunks = chunks.saturating_add(1);
            batches_written = batches_written.saturating_add(chunk.batches_written);
            rows_written = rows_written.saturating_add(chunk.rows_written);
            sst_files_written = sst_files_written.saturating_add(materialized.sst_files);
            sst_bytes_written = sst_bytes_written.saturating_add(materialized.sst_bytes);
            tracing::info!(
                code = "CALYX_ASTER_CHECKPOINT_MATERIALIZED_OFF_COMMIT_LOCK",
                operation,
                base_durable_seq = prepared.base_durable_seq,
                first_seq = prepared.first_seq,
                last_seq = prepared.last_seq,
                batches = prepared.batch_count(),
                rows = prepared.rows,
                source_bytes = prepared.bytes,
                sst_files = materialized.sst_files,
                sst_bytes = materialized.sst_bytes,
                materialize_ms,
                remaining_batches = chunk.remaining_batches,
                "materialized immutable checkpoint SSTs with the global durable commit lock released"
            );
            if chunk.remaining_batches == 0 {
                break;
            }
            if chunks >= PACED_CHECKPOINT_MAX_HOLDS {
                return Err(CalyxError {
                    code: "CALYX_ASTER_CHECKPOINT_DRAIN_UNCONVERGED",
                    message: format!(
                        "paced checkpoint drain for {operation} used its full budget of {PACED_CHECKPOINT_MAX_HOLDS} off-lock materialization chunks ({batches_written} batches, {rows_written} rows, {} ms) and {} batches are still staged; incoming commits are outrunning durable checkpoint materialization",
                        started.elapsed().as_millis(),
                        chunk.remaining_batches
                    ),
                    remediation: "throttle the write source or provision faster durable storage; retry maintenance once the staged backlog drains",
                });
            }
            self.yield_durable_commit_lock_to_writers();
        }
        if batches_written > 0 || chunks > 1 || checkpoint_lock_wait_ms > 0 {
            tracing::info!(
                code = "CALYX_ASTER_CHECKPOINT_DRAIN_PACED",
                operation,
                chunks,
                batches_written,
                rows_written,
                sst_files_written,
                sst_bytes_written,
                checkpoint_lock_wait_ms,
                max_reserve_elapsed_ms,
                max_materialize_ms,
                max_publish_elapsed_ms,
                elapsed_ms = started.elapsed().as_millis(),
                max_batches_per_chunk = durable::CHECKPOINT_DRAIN_MAX_BATCHES,
                "materialized staged durable checkpoints with physical SST I/O outside the global commit lock"
            );
        }
        Ok(())
    }

    /// Hands the durable commit lock to queued writers between bounded
    /// maintenance units. Without this, an unfair mutex lets the draining
    /// thread immediately re-acquire and the pacing buys writers nothing.
    fn yield_durable_commit_lock_to_writers(&self) {
        if self.durable_commit_lock_waiters() > 0 {
            std::thread::sleep(COMMIT_LOCK_HANDOFF_PAUSE);
        } else {
            std::thread::yield_now();
        }
    }

    /// Synchronizes the WAL batcher and materializes pending durable
    /// checkpoints without evicting router memtables.
    ///
    /// Durable checkpoint SSTs are the manifest/WAL-recycling authority. A
    /// router memtable is a serving cache over the same committed rows and is
    /// independently flushed by its byte cap. Keeping these boundaries
    /// separate prevents a caller requesting durability from creating one
    /// tiny router SST per logical flush.
    pub fn checkpoint(&self) -> Result<()> {
        self.drain_checkpoints_paced("periodic checkpoint")
    }

    /// Waits until every submitted group-commit WAL append has reached its
    /// fsynced acknowledgement boundary without materializing checkpoint SSTs.
    ///
    /// Each acknowledged write is already durable in the WAL. This barrier is
    /// therefore the correct implementation for callers that ask only to sync
    /// pending writes; checkpoint SST publication remains owned by maintenance,
    /// WAL recycling, and deterministic close.
    /// Takes no commit lock: this is a barrier on the group-commit batcher
    /// thread, not a vault mutation. Holding the lock across it parked every
    /// writer behind that thread's queue depth for as long as the queue took to
    /// drain, which is unbounded under load and bought nothing — the batcher
    /// serializes its own work, and this method mutates no vault state (#1832).
    pub fn sync_wal(&self) -> Result<()> {
        self.ensure_writeable("sync WAL")?;
        if let Some(durable) = &self.durable {
            durable.sync_wal()?;
        }
        Ok(())
    }

    pub(crate) fn checkpoint_locked(&self) -> Result<()> {
        self.ensure_writeable("checkpoint")?;
        if let Some(durable) = &self.durable {
            let Some(_checkpoint_guard) =
                crate::file_lock::FileLockGuard::try_acquire(&durable.checkpoint_lock_path())?
            else {
                return Err(CalyxError::backpressure(
                    "exclusive checkpoint could not acquire the checkpoint publisher lock without waiting while the global commit lock is held; retry after the active off-lock publisher completes",
                ));
            };
            durable.flush()?;
        }
        Ok(())
    }

    pub(crate) fn flush_locked(&self) -> Result<()> {
        self.checkpoint_locked()?;
        self.rows.flush_all_cfs()?;
        Ok(())
    }

    /// Flushes pending checkpoints, advances the durable manifest floor, then
    /// reclaims at most `max_segments.min(fsync_budget)` WAL segments whose
    /// complete sequence range is covered by that manifest.
    pub fn recycle_durable_wal_once(
        &self,
        max_segments: usize,
        fsync_budget: usize,
    ) -> Result<WalRecycleReport> {
        if max_segments == 0 || fsync_budget == 0 {
            return Err(CalyxError::disk_pressure(
                "WAL recycle limits must both be non-zero",
            ));
        }
        self.drain_checkpoints_paced("WAL recycle preflight")?;
        self.with_durable_commit_lock(|| {
            let Some(durable) = &self.durable else {
                return Ok(WalRecycleReport::default());
            };
            let manifest_durable_seq = self.verified_durable_coverage_seq(durable)?;
            let report =
                durable.recycle_durable_wal_segments(max_segments, fsync_budget)?;
            if report.newest_durable_seq != manifest_durable_seq {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "WAL recycler used durable sequence {} but locked manifest coverage is {manifest_durable_seq}",
                    report.newest_durable_seq
                )));
            }
            Ok(report)
        })
    }

    /// Pins an explicit reader lease tracked for oldest-pinned-seq accounting.
    ///
    /// Unlike scoped vault-internal snapshot handles, explicit pins remain in
    /// the store lease registry after one read call, until
    /// [`Self::release_reader`] or lease expiry.
    pub fn pin_reader(&self, freshness: Freshness, max_age_ms: u64) -> Snapshot {
        self.rows.pin_snapshot(freshness, &self.clock, max_age_ms)
    }

    /// Pins a reader with the exact search-input watermark for one panel.
    pub fn pin_reader_for_panel(
        &self,
        panel_version: u32,
        freshness: Freshness,
        max_age_ms: u64,
    ) -> Result<Snapshot> {
        self.rows
            .pin_snapshot_for_panel(panel_version, freshness, &self.clock, max_age_ms)
    }

    /// Releases an explicit reader lease; returns whether it was still live.
    pub fn release_reader(&self, lease_id: u64) -> bool {
        self.rows.release_lease(lease_id)
    }

    /// Pins a reader lease at a historical `seq` (time-travel) and returns its
    /// lease id, which the caller must release with [`Self::release_reader`].
    pub fn pin_reader_at(&self, seq: Seq, max_age_ms: u64) -> u64 {
        self.rows
            .pin_snapshot_at(seq, Freshness::FreshDerived, &self.clock, max_age_ms)
            .lease()
            .id()
    }

    /// Opens a time-travel snapshot as of wall-clock `t_millis` (PRD `17 §8`).
    pub fn as_of(&self, t_millis: u64) -> Result<crate::timetravel::TimeTravelSnapshot<'_, C>> {
        crate::timetravel::TimeTravelSnapshot::open(self, t_millis)
    }

    /// Collects the aggregate resource status for this vault (PRD 18 §4).
    ///
    /// `vault_dir` is the durable root this vault was opened from; `vram` is
    /// the VRAM budget section sourced from the vault Anneal budget config.
    pub fn resource_status(
        &self,
        vault_dir: &Path,
        vram: VramBudgetStatus,
    ) -> Result<ResourceStatus> {
        collect_resource_status(vault_dir, vram, &self.rows, self.clock.now())
    }

    pub fn install_read_barrier(&self, barrier: ReadBarrier) {
        self.rows.install_read_barrier(barrier);
    }

    pub fn remove_read_barrier(&self, id: &str) -> bool {
        self.rows.remove_read_barrier(id)
    }

    pub fn read_barriers(&self) -> Vec<ReadBarrier> {
        self.rows.read_barriers()
    }
}
