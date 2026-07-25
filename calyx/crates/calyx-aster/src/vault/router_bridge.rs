use super::{AsterVault, DEFAULT_LEASE_MS, VaultRecoveryReport};
use crate::cf::CfRouter;
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
            commit_lock: std::sync::Mutex::new(()),
            commit_lock_waiters: std::sync::atomic::AtomicUsize::new(0),
            recurrence_write_lock: std::sync::Mutex::new(()),
            ledger_state_reconciliation_required: std::sync::atomic::AtomicBool::new(false),
            post_commit_error_seq: std::sync::atomic::AtomicU64::new(0),
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
}
