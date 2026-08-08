use super::{AsterVault, DEFAULT_LEASE_MS, VaultRecoveryReport};
use crate::cf::{CfRouter, ColumnFamily};
use crate::dedup::DedupPolicy;
use crate::mvcc::{Freshness, Snapshot, VersionedCfStore};
use crate::sst::SstSummary;
use crate::timetravel::RetentionHorizon;
use calyx_core::{Clock, Result, Seq};

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn with_clock_and_router(
        vault_id: calyx_core::VaultId,
        vault_salt: impl Into<Vec<u8>>,
        clock: C,
        router: CfRouter,
    ) -> Self {
        Self {
            vault_id,
            vault_salt: vault_salt.into(),
            clock,
            rows: VersionedCfStore::new_with_router(0, router),
            durable: None,
            dedup_policy: DedupPolicy::default(),
            retention_horizon: std::sync::Mutex::new(RetentionHorizon::default()),
            ledger_hook: None,
            read_only: false,
            // A router handed in here is the caller's whole world; there is no
            // partial-open decision for the read path to remember (#1969).
            selected_cfs: None,
            commit_lock: std::sync::Mutex::new(()),
            commit_lock_waiters: std::sync::atomic::AtomicUsize::new(0),
            recurrence_write_lock: std::sync::Mutex::new(()),
            ledger_state_reconciliation_required: std::sync::atomic::AtomicBool::new(false),
            post_commit_error_seq: std::sync::atomic::AtomicU64::new(0),
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

    pub fn pin_stale_snapshot(&self, max_lag: Seq) -> Snapshot {
        self.rows.pin_snapshot(
            Freshness::StaleOk { max_lag },
            &self.clock,
            DEFAULT_LEASE_MS,
        )
    }

    pub fn flush_all_cfs(&self) -> Result<Vec<SstSummary>> {
        self.rows.flush_all_cfs()
    }

    /// Reports whether this handle serves latest reads from the CF router
    /// (`restore_mvcc_rows: false`) rather than the in-memory MVCC row table.
    ///
    /// See [`VersionedCfStore::router_latest_readback`] — this exists so a
    /// caller that intends to exercise the router-backed read branch can prove
    /// it reached it instead of assuming the open mode it asked for is the mode
    /// it got (#1954).
    pub fn router_latest_readback(&self) -> bool {
        self.rows.router_latest_readback()
    }

    /// Visible rows of `cf` held by the MVCC row table alone, router excluded.
    ///
    /// See [`VersionedCfStore::latest_row_count_table_only`]. This is the
    /// readback that lets a harness prove the full-restore invariant the #1978
    /// router gate rests on, instead of taking it on the recovery code's word.
    pub fn latest_row_count_table_only(&self, cf: ColumnFamily) -> usize {
        self.rows.latest_row_count_table_only(cf)
    }
}
