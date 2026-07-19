//! Aster `VaultStore` implementation over the PH08 MVCC CF table.

mod anchor_codec;
mod anchor_compact;
mod anchor_merge;
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
mod temporal_xterm;
use crate::cf::{CfRouter, ColumnFamily, KeyRange};
use crate::dedup::DedupPolicy;
use crate::mvcc::{Freshness, ReadBarrier, Snapshot, VersionedCfStore};
use crate::resource::{ResourceStatus, VramBudgetStatus, collect_resource_status};
use crate::timetravel::RetentionHorizon;
use crate::vault::durable::DurableVault;
use crate::vault::ledger_hook::AsterLedgerHook;
use crate::wal::{TornTail, WalRecycleReport};
use calyx_core::{CalyxError, Clock, Constellation, CxId, Result, Seq, SystemClock, VaultId};
use std::{path::Path, sync::Mutex};

pub use anchor_compact::{AnchorCompactionConflict, AnchorCompactionReport};
pub use commit::CALYX_DURABLE_COMMIT_RECONCILIATION_REQUIRED;
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
pub use quota::{CALYX_QUOTA_EXCEEDED, QuotaConfig, QuotaGuard};
pub use slot_column::{
    SlotColumnManifest, SlotColumnMaterialization, SlotColumnReadback, SlotColumnRow,
    read_materialized_slot_column,
};
pub use store::{PutDisposition, PutOutcome};
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
    recurrence_write_lock: Mutex<()>,
    recovery_report: VaultRecoveryReport,
    residency: Option<crate::residency::Residency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRecoveryReport {
    pub last_recovered_seq: Seq,
    pub torn_tail: Option<TornTail>,
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
            recurrence_write_lock: Mutex::new(()),
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
        if rows.is_empty() {
            return Ok(self.latest_seq());
        }
        self.commit_rows(&rows)
    }

    /// Writes one raw CF row through the WAL-backed batch path.
    pub fn write_cf(&self, cf: ColumnFamily, key: Vec<u8>, value: Vec<u8>) -> Result<Seq> {
        self.write_cf_batch([(cf, key, value)])
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
        self.with_durable_commit_lock(|| self.flush_locked())
    }

    pub(crate) fn flush_locked(&self) -> Result<()> {
        self.ensure_writeable("flush")?;
        if let Some(durable) = &self.durable {
            durable.flush()?;
        }
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
        self.with_durable_commit_lock(|| {
            self.flush_locked()?;
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
