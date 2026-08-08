//! Aster `VaultStore` implementation over the PH08 MVCC CF table.

mod anchor_codec;
mod anchor_compact;
mod anchor_merge;
mod backup;
pub mod base_rewrite;
mod batch_ingest;

/// Expected physical revision of the Registry lifecycle row guarded by an
/// atomic derived-snapshot publication.
#[derive(Clone, Debug)]
pub struct DerivedRegistryRevisionGuard {
    pub key: Vec<u8>,
    pub expected_revision: Option<[u8; 32]>,
}
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
mod raw_commitment;
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
    AsterBackupExclusion, AsterBackupFile, AsterBackupPinnedManifest, AsterBackupReport,
    AsterBackupToleratedAbsence, CALYX_ASTER_BACKUP_IO, CALYX_ASTER_BACKUP_MANIFEST_PIN_FAILED,
    CALYX_ASTER_BACKUP_MANIFEST_REF_MISSING, CALYX_ASTER_BACKUP_NOT_DURABLE,
    CALYX_ASTER_BACKUP_PIN_COVERAGE_REGRESSED, CALYX_ASTER_BACKUP_TARGET_INVALID,
    EXCLUDED_RUNTIME_FILES, REGENERABLE_DIRS,
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
pub use ledger_append::{
    AsterLedgerChainVerification, AsterProvenanceReproduction, AsterRawCommitmentVerification,
};
/// The one page size every bounded maintenance walk over a whole family uses.
///
/// Declared in one place for #1977: seven callers were migrated off whole-family
/// materialization at once, and a per-caller constant would have made the
/// row-guard census impossible to reason about across them. See
/// [`orphan_slot_gc::ORPHAN_SLOT_GC_PAGE_ROWS`] for how 256 was chosen.
pub(crate) use orphan_slot_gc::ORPHAN_SLOT_GC_PAGE_ROWS;
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
    /// Column families this handle actually opened, or `None` when it opened
    /// every one (issue #1969).
    ///
    /// A partially-opened handle is a read-only inspection convenience: it skips
    /// the recovery cost of families the caller does not need. Before this
    /// field, the read path had no memory of that decision, so asking such a
    /// handle for an unopened family resolved through `levels.get(&cf)` to
    /// `unwrap_or_default()` and answered **zero rows** — indistinguishable from
    /// a family that is genuinely empty. That produced a confident wrong number
    /// in a readback harness, and its reverse polarity is worse: a phase
    /// verifying that a purge or retire landed would read `0`, report success,
    /// and never have looked at the data at all.
    ///
    /// RocksDB refuses to open a database at all unless every column family is
    /// named. Calyx allows the partial open on purpose, so it pays for that
    /// choice here instead: a read outside the selected set fails closed naming
    /// the family and the set, and never resolves to empty.
    selected_cfs: Option<BTreeSet<ColumnFamily>>,
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
    /// Self-calibrating gate and running census for durable-commit duration
    /// (issue #1946). Per-vault rather than process-global so an isolated
    /// vault's commits cannot shift the live daemon's baseline.
    commit_stage_observer: commit::CommitStageObserver,
    /// Held-open, pre-allocated writers for the two derived Ledger
    /// projections (issue #1947).
    ///
    /// Lazily created on the first commit that publishes one, because the
    /// vault root is only known once a durable vault exists. Per-vault, so an
    /// isolated vault's handles and cached anchors can never be confused with
    /// the live daemon's.
    ledger_projections: Mutex<Option<crate::ledger_head::LedgerProjections>>,
    /// Set exactly once, by [`Self::declare_close_intent`], to the reason this
    /// vault is closing (issue #2100).
    ///
    /// # What it fences, and why a flag is the right shape
    ///
    /// A close has to reach the flush and the final record; it must not be able
    /// to queue behind maintenance work that was admitted *after* the close was
    /// commanded. Nothing in the vault used to distinguish those two, so
    /// `compact_native_fanout_once` on the close path took the ordinary fair
    /// admission with a 120 s budget and could sit behind an arbitrary
    /// concurrently-admitted pass.
    ///
    /// Once this is set:
    ///
    /// * every **new** native-compaction admission is refused with
    ///   `CALYX_ASTER_VAULT_CLOSING`, whatever its purpose, except the close's
    ///   own (see [`crate::vault::compaction_bridge`]);
    /// * every fair-admission waiter already parked on that lock aborts instead
    ///   of burning the rest of its budget;
    /// * every maintenance acquisition of the durable commit lock
    ///   ([`Self::with_durable_commit_lock_maintenance`]) is refused, so no
    ///   background lane can take the one lock that serialises vault writes
    ///   after the close has started.
    ///
    /// Set-once rather than a toggle: a close cannot be un-commanded, and a
    /// fence that could be cleared would let a maintenance lane race the close
    /// back onto the lock. `OnceLock` carries the reason as well as the bit, so
    /// a refusal names *which* close fenced it.
    close_intent: std::sync::OnceLock<&'static str>,
}

/// What each named step of [`AsterVault::close_teardown`] cost (issue #2100).
///
/// Carried out of the teardown rather than only logged so the caller can put the
/// same split on its own close record — an operator reading a `previous_shutdown`
/// verdict should not have to correlate two log streams to learn which step of
/// the close ran long.
#[derive(Debug)]
pub struct VaultTeardownReport {
    /// The close reason this teardown ran under.
    pub reason: &'static str,
    /// Sealed memtables still awaiting their SST when the teardown began.
    ///
    /// Before #2100 these were collected by `RouterFlusher::drop`'s unbudgeted,
    /// unlogged join. A non-zero value here is the exact backlog the close had
    /// to absorb.
    pub sealed_outstanding_before: usize,
    /// Draining that backlog under the flusher's own budget.
    pub flush_drain_ms: u128,
    /// Releasing the MVCC row store, its version chains and the router.
    pub rows_teardown_ms: u128,
    /// Releasing the durable handle (WAL batcher, staged checkpoints, crypto).
    pub durable_teardown_ms: u128,
    /// Releasing the Ledger projections and hook.
    pub residual_teardown_ms: u128,
    /// Whole teardown, end to end.
    pub total_ms: u128,
    /// The first background flush failure, if the drain surfaced one.
    ///
    /// Held rather than returned as `Err` so the timings are never lost to the
    /// failure: a close that failed its drain is exactly the close whose cost
    /// breakdown matters most.
    pub drain_error: Option<CalyxError>,
}

impl VaultTeardownReport {
    /// The teardown's own verdict, separated from its measurements.
    ///
    /// # Errors
    ///
    /// Returns the first background flush failure the drain surfaced.
    pub fn verdict(&self) -> Result<()> {
        self.drain_error.clone().map_or(Ok(()), Err)
    }
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
/// Stable error code for attempts to forge Aster's internal raw-commitment CF.
pub const CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN: &str =
    "CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN";
/// Stable error code for a read addressed to a column family this handle never
/// opened (issue #1969).
///
/// Named separately from every "empty" outcome on purpose: the whole point is
/// that a caller can tell "this family holds nothing" from "this handle cannot
/// see this family", which a `0` cannot express.
pub const CALYX_ASTER_CF_NOT_SELECTED: &str = "CALYX_ASTER_CF_NOT_SELECTED";

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
    if let Some((row_index, row)) = rows
        .iter()
        .enumerate()
        .find(|(_, row)| row.cf == ColumnFamily::RawCommitment)
    {
        return Err(CalyxError {
            code: CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN,
            message: format!(
                "{operation}: raw commitment mutation is reserved: row_index={row_index} key_len={} value_len={}",
                row.key.len(),
                row.value.len()
            ),
            remediation: "write application rows through the normal Aster commit APIs; Aster derives the sequence-bound commitment row atomically and callers cannot supply or overwrite it",
        });
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
    if let Some((guard_index, guard)) = guards
        .iter()
        .enumerate()
        .find(|(_, guard)| guard.cf == ColumnFamily::RawCommitment)
    {
        return Err(CalyxError {
            code: CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN,
            message: format!(
                "{operation}: raw commitment revision guard is reserved: guard_index={guard_index} key_len={}",
                guard.key.len()
            ),
            remediation: "do not read/modify/write Aster's internal commitment rows; use verify_ledger_chain for their fail-closed readback",
        });
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
            // In-memory vault: every column family is reachable, so a miss
            // genuinely means empty (#1969).
            selected_cfs: None,
            dedup_policy: DedupPolicy::default(),
            retention_horizon: Mutex::new(RetentionHorizon::default()),
            ledger_hook: None,
            read_only: false,
            commit_lock: Mutex::new(()),
            commit_lock_waiters: AtomicUsize::new(0),
            recurrence_write_lock: Mutex::new(()),
            ledger_state_reconciliation_required: AtomicBool::new(false),
            post_commit_error_seq: AtomicU64::new(0),
            commit_stage_observer: Default::default(),
            ledger_projections: Default::default(),
            close_intent: std::sync::OnceLock::new(),
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

    /// One column family's exact `O(1)` change signal (#2139).
    ///
    /// [`Self::latest_seq`] moves on every commit to *any* family, which makes
    /// it a usable invalidation key only for a vault that is entirely
    /// quiescent — a far stronger and rarer condition than "this one family did
    /// not change", and the reason the #2114 count memo never reused anything on
    /// a busy vault. See [`crate::mvcc::CfChangeSignal`] for what the fields
    /// license.
    pub fn cf_change_signal(&self, cf: ColumnFamily) -> crate::mvcc::CfChangeSignal {
        self.rows.cf_change_signal(cf)
    }

    /// Greatest committed sequence that wrote a row into `cf`, in `O(1)`.
    pub fn latest_seq_for_cf(&self, cf: ColumnFamily) -> Seq {
        self.rows.latest_seq_for_cf(cf)
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

    /// Per-site row-table read-guard tallies since this vault was opened.
    ///
    /// See [`crate::mvcc::VersionedCfStore::row_guard_census`]. Exposed on the
    /// vault so the daemon can report it without reaching into the MVCC store.
    pub fn row_guard_census(&self) -> Vec<crate::mvcc::RowGuardSiteCensus> {
        self.rows.row_guard_census()
    }

    /// What the background SST flusher has written, failed, and waited on.
    ///
    /// `written` is the number that has to agree with the growth in
    /// `flush-*.sst` files on disk: two independent counters, one from the
    /// process and one from the filesystem, and a disagreement is a deferred
    /// write that was actually dropped (#1951).
    pub fn flush_status(&self) -> crate::mvcc::FlushStatus {
        self.rows.flush_status()
    }

    /// Waits until every sealed memtable has been written and installed,
    /// surfacing the first background failure. See
    /// [`crate::mvcc::VersionedCfStore::drain_pending_flushes`].
    ///
    /// # Errors
    ///
    /// Returns the first background write failure, or an error when the queue
    /// does not drain inside its budget.
    pub fn drain_pending_flushes(&self) -> Result<()> {
        self.rows.drain_pending_flushes()
    }

    /// Tears the vault down in **named, individually timed steps** instead of
    /// one opaque `drop` (issue #2100).
    ///
    /// # Why this exists
    ///
    /// `SynapseCalyx::close` used to log `SYNAPSE_CALYX_VAULT_FLUSHED`, run
    /// `drop(vault)`, and log the next line. On the production deploy that
    /// #2100 was opened for, and on the *successful* close one daemon
    /// generation earlier, that single unlogged statement took **63 and 87+
    /// seconds** with not one intervening log line — long enough for the deploy
    /// drain to escalate and kill the process before the graceful exit record
    /// was written. The region could not be attributed after the fact because
    /// nothing in it emits anything, and the issue's own reading of the evidence
    /// (a stall behind the durable commit lock) is falsified by the same log:
    /// the last `CALYX_ASTER_DURABLE_COMMIT_LOCK_SLOW` before the gap reported
    /// `wait_ms=0 hold_ms=1478`.
    ///
    /// So the teardown names its steps. `vault_close_bound_fsv` measures them on
    /// an isolated vault (where the row/version teardown is 0.29 us per resident
    /// MVCC version, scaling 2.04x for 2x versions); this makes the same split
    /// readable on the deployment host, where the absolute number is two orders
    /// of magnitude larger and had no attribution at all.
    ///
    /// The explicit flusher drain is also a *bound*, not only a measurement: the
    /// sealed memtables that the final flush produced were previously written by
    /// the flusher thread and collected by `RouterFlusher::drop`'s join, which
    /// has no budget and no telemetry. `drain_pending_flushes` has both.
    ///
    /// # Errors
    ///
    /// Returns the first background flush failure, or a failure to drain inside
    /// the flusher's budget. The teardown still completes: the report is
    /// returned alongside the verdict so the caller can log what the close cost
    /// even when the drain failed.
    #[must_use]
    pub fn close_teardown(self, reason: &'static str) -> VaultTeardownReport {
        let started = Instant::now();
        let outstanding_before = self.rows.flush_status().outstanding;
        let drain_started = Instant::now();
        let drain = self.rows.drain_pending_flushes();
        let flush_drain_ms = drain_started.elapsed().as_millis();
        let drain_error = drain.err();

        let Self {
            vault_id: _,
            vault_salt: _,
            clock: _,
            rows,
            durable,
            dedup_policy: _,
            retention_horizon: _,
            ledger_hook,
            read_only: _,
            selected_cfs: _,
            commit_lock: _,
            commit_lock_waiters: _,
            recurrence_write_lock: _,
            ledger_state_reconciliation_required: _,
            post_commit_error_seq: _,
            recovery_report: _,
            residency: _,
            commit_stage_observer: _,
            ledger_projections,
            close_intent: _,
        } = self;

        // Order matters and is the same order the compiler would have used:
        // the row store owns the flusher thread and must go before the router it
        // writes into, and the durable handle must outlive both.
        let rows_started = Instant::now();
        drop(rows);
        let rows_teardown_ms = rows_started.elapsed().as_millis();

        let durable_started = Instant::now();
        drop(durable);
        let durable_teardown_ms = durable_started.elapsed().as_millis();

        let residual_started = Instant::now();
        drop(ledger_projections);
        drop(ledger_hook);
        let residual_teardown_ms = residual_started.elapsed().as_millis();

        let report = VaultTeardownReport {
            reason,
            sealed_outstanding_before: outstanding_before,
            flush_drain_ms,
            rows_teardown_ms,
            durable_teardown_ms,
            residual_teardown_ms,
            total_ms: started.elapsed().as_millis(),
            drain_error,
        };
        tracing::info!(
            code = "CALYX_ASTER_VAULT_TEARDOWN_TIMED",
            reason,
            sealed_outstanding_before = report.sealed_outstanding_before,
            flush_drain_ms = report.flush_drain_ms,
            rows_teardown_ms = report.rows_teardown_ms,
            durable_teardown_ms = report.durable_teardown_ms,
            residual_teardown_ms = report.residual_teardown_ms,
            total_ms = report.total_ms,
            drain_ok = report.drain_error.is_none(),
            "tore the Calyx vault down in named steps; this region was one unlogged `drop` before \
             #2100 and carried 63-87 s of the production close"
        );
        report
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

    /// Column families this handle opened, or `None` when it opened every one.
    #[must_use]
    pub fn selected_cfs(&self) -> Option<Vec<ColumnFamily>> {
        self.selected_cfs
            .as_ref()
            .map(|cfs| cfs.iter().copied().collect())
    }

    /// Refuses a read addressed to a column family this handle never opened
    /// (issue #1969).
    ///
    /// Without this, such a read resolves to zero rows, which a caller cannot
    /// distinguish from an empty family. The paging path already gets this right
    /// — it fails closed with `CALYX_ASTER_SST_PAGE_INDEX_MISSING` naming the
    /// file rather than answering zero — and this is the same discipline applied
    /// to the family selection itself.
    ///
    /// Costs one `BTreeSet` lookup, and only on handles that were opened
    /// partially: a fully-opened vault holds `None` here and returns
    /// immediately. `selected_cfs` requires `read_only=true`, so no write path
    /// can reach this.
    fn assert_cf_selected(&self, cf: ColumnFamily, operation: &'static str) -> Result<()> {
        let Some(selected) = &self.selected_cfs else {
            return Ok(());
        };
        if selected.contains(&cf) {
            return Ok(());
        }
        let selected_list = selected
            .iter()
            .map(|cf| format!("{cf:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(CalyxError {
            code: CALYX_ASTER_CF_NOT_SELECTED,
            message: format!(
                "{operation} addressed column family {cf:?}, which this read-only handle did not \
                 open; it opened only [{selected_list}]. Answering zero rows here would be \
                 indistinguishable from the family being empty"
            ),
            remediation: "reopen the vault with this column family in selected_cfs (or open every \
                          family); do not read the zero as evidence that the family is empty or \
                          that a delete landed",
        })
    }

    /// Reads one raw CF row from one atomic view of the latest committed state.
    pub fn read_cf_latest(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.assert_cf_selected(cf, "read_cf_latest")?;
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
        self.assert_cf_selected(cf, "read_cf_latest_revisioned")?;
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
        for read in reads {
            self.assert_cf_selected(read.cf, "read_cf_batch_latest")?;
        }
        self.rows.read_batch_latest(reads)
    }

    /// Reads one raw CF row at `snapshot`.
    pub fn read_cf_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.assert_cf_selected(cf, "read_cf_at")?;
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
        self.assert_cf_selected(cf, "read_cf_snapshot")?;
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
    /// Validates that an appender's requested head is already represented by
    /// the committed Ledger row and the durable head projection.
    ///
    /// # Errors
    ///
    /// Returns a reconciliation-required error when the row, encoded entry,
    /// or durable head projection does not prove the requested head.
    pub fn validate_committed_ledger_head_anchor(
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

    /// Reads the durable Ledger head and verifies its exact physical tip row.
    ///
    /// A durable vault with Ledger rows but no head projection fails closed;
    /// callers must not recover an append position from an unverified latest
    /// row. In-memory vaults return `None`, which preserves the ledger
    /// contract's complete-chain recovery path for non-durable stores.
    ///
    /// # Errors
    ///
    /// Returns a Ledger integrity error when the projection is absent,
    /// malformed, or disagrees with its committed tip row.
    pub fn verified_ledger_head_anchor(&self) -> Result<Option<LedgerHeadAnchor>> {
        self.with_durable_commit_lock(|| {
            let Some(durable) = &self.durable else {
                return Ok(None);
            };
            let anchor = crate::ledger_head::read_head_anchor(durable.root())?;
            if let Some(anchor) = anchor {
                self.validate_ledger_head_tip_row_locked(&anchor, "durable Ledger head")?;
                return Ok(Some(anchor));
            }

            let greatest = self.predecessor_cf_at(
                self.latest_seq(),
                ColumnFamily::Ledger,
                &crate::cf::ledger_key(0),
                &crate::cf::ledger_key(u64::MAX),
            )?;
            let Some((key, _)) = greatest else {
                return Ok(None);
            };
            let key: [u8; 8] = key.try_into().map_err(|key: Vec<u8>| {
                CalyxError::ledger_corrupt(format!(
                    "greatest durable Ledger key has {} bytes, expected 8",
                    key.len()
                ))
            })?;
            let seq = u64::from_be_bytes(key);
            let height = seq.checked_add(1).ok_or_else(|| {
                CalyxError::ledger_chain_broken(
                    "durable Ledger contains sequence u64::MAX and its head height cannot be represented",
                )
            })?;
            Err(crate::ledger_head::missing_head_anchor(
                durable.root(),
                height,
            ))
        })
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
        self.assert_cf_selected(cf, "scan_cf_at")?;
        let snapshot = self.snapshot_handle(snapshot);
        self.rows.scan_cf_at(snapshot.snapshot(), cf, &self.clock)
    }

    /// Scans visible raw CF rows for a pinned lease; use `scan_cf_pages_snapshot` for large data.
    pub fn scan_cf_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.assert_cf_selected(cf, "scan_cf_snapshot")?;
        self.rows.scan_cf_at(snapshot, cf, &self.clock)
    }

    /// Returns every key changed after `after_exclusive` and no later than the
    /// pinned snapshot, including tombstoned keys. The MVCC layer fails closed
    /// when latest-only recovery cannot prove complete history for the range.
    pub fn changed_cf_keys_after_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        after_exclusive: Seq,
    ) -> Result<Vec<Vec<u8>>> {
        self.assert_cf_selected(cf, "changed_cf_keys_after_snapshot")?;
        self.rows
            .changed_keys_after_at(snapshot, cf, after_exclusive, &self.clock)
    }

    /// Returns the `Base` changed-key delta scoped to one exact panel version,
    /// with the composition that produced it (#1901).
    ///
    /// Every caller that bounds or reports a panel-scoped reconciliation delta
    /// must use this rather than the whole-`Base`
    /// [`Self::changed_cf_keys_after_snapshot`]: `Base` is shared by every
    /// panel, so an unscoped count charges one panel's budget for another
    /// panel's ingest.
    pub fn changed_base_keys_after_snapshot_for_panel(
        &self,
        snapshot: Snapshot,
        after_exclusive: Seq,
        panel_version: u32,
    ) -> Result<crate::mvcc::PanelScopedChangedKeys> {
        self.assert_cf_selected(
            ColumnFamily::Base,
            "changed_base_keys_after_snapshot_for_panel",
        )?;
        self.rows.changed_base_keys_after_at_for_panel(
            snapshot,
            after_exclusive,
            panel_version,
            &self.clock,
        )
    }

    /// Scans visible raw CF rows from one atomic latest committed view.
    pub fn scan_cf_latest(&self, cf: ColumnFamily) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.assert_cf_selected(cf, "scan_cf_latest")?;
        self.rows.scan_cf_latest(cf)
    }

    /// Counts visible rows in one CF at the latest committed state.
    ///
    /// Equal to `scan_cf_latest(cf)?.len()` without materialising any value
    /// (#1952). Prefer this wherever only the count is wanted: `scan_cf_latest`
    /// holds the vault-wide row-table read guard for the whole scan, and #1950
    /// measured that hold reaching 1.4 s on this CF set, which stalls every
    /// committing writer for the duration.
    pub fn count_cf_latest(&self, cf: ColumnFamily) -> Result<usize> {
        self.assert_cf_selected(cf, "count_cf_latest")?;
        self.rows.count_cf_latest(cf)
    }

    /// Scans visible raw CF rows in a key range at `snapshot`.
    pub fn scan_cf_range_at(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.assert_cf_selected(cf, "scan_cf_range_at")?;
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
        self.assert_cf_selected(cf, "scan_cf_range_snapshot")?;
        self.rows.scan_cf_range_at(snapshot, cf, range, &self.clock)
    }

    /// Scans visible raw CF rows in a range from one atomic latest committed view.
    pub fn scan_cf_range_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.assert_cf_selected(cf, "scan_cf_range_latest")?;
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
        self.assert_cf_selected(cf, "scan_cf_range_page_latest")?;
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
        self.assert_cf_selected(cf, "scan_cf_range_keys_at")?;
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
        self.assert_cf_selected(cf, "scan_cf_range_page_at")?;
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
        self.assert_cf_selected(cf, "predecessor_cf_at")?;
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
                if let Some(seal) = durable.pending_raw_commitment_seal()? {
                    let ledger_ref = self.commit_rows_with_ledger_entry_locked(
                        Vec::new(),
                        calyx_ledger::EntryKind::BatchCommitment,
                        calyx_ledger::SubjectId::Query(
                            raw_commitment::RAW_COMMITMENT_SUBJECT.to_vec(),
                        ),
                        raw_commitment::encode_seal(&seal),
                        calyx_ledger::ActorId::Service("calyx-aster".to_owned()),
                    )?;
                    tracing::info!(
                        code = "CALYX_ASTER_RAW_COMMITMENT_COHORT_SEALED",
                        operation,
                        first_commit_seq = seal.first_seq,
                        last_commit_seq = seal.last_seq,
                        commitment_count = seal.commitment_count,
                        ledger_seq = ledger_ref.seq,
                        ledger_hash = ?ledger_ref.hash,
                        "sealed the staged raw-commitment cohort into the append-only Ledger before checkpoint publication"
                    );
                }
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
                durable.advance_panel_content_watermarks_to_at_least(
                    &self.rows.panel_content_seqs_snapshot()?,
                )?;
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
        self.drain_checkpoints_paced("periodic checkpoint")?;
        // The background flusher's barrier (#1951).
        //
        // Router-flush SSTs are not the recovery authority — the WAL is, and
        // `router_flush_durability_window_fsv` proved it by deleting every one
        // of them and still recovering 460/460 keys. So this is not a
        // durability requirement; it is an honesty one. A checkpoint is the
        // moment the vault asserts its physical projection is materialized, and
        // returning `Ok` here while an SST write is still outstanding — or has
        // *failed* and been logged to nobody who checks — would make that
        // assertion false.
        //
        // `drain` surfaces the first background failure rather than swallowing
        // it, so a write that failed after its commit returned fails the next
        // checkpoint instead of disappearing.
        self.rows.drain_pending_flushes()
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
            durable.advance_panel_content_watermarks_to_at_least(
                &self.rows.panel_content_seqs_snapshot()?,
            )?;
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
        // Maintenance lane: fenced once a close is declared (#2100).
        self.with_durable_commit_lock_maintenance("durable WAL recycle", || {
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
