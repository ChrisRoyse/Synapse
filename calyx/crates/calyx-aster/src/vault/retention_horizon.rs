use super::{AsterVault, encode};
use crate::timetravel::RetentionHorizon;
use calyx_core::{CalyxError, Clock, Result};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use serde_json::json;

const RETENTION_HORIZON_SUBJECT: &[u8] = b"timetravel_retention_horizon";

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn retention_horizon(&self) -> RetentionHorizon {
        self.retention_horizon
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn set_retention_horizon(&self, horizon: RetentionHorizon) -> Result<()> {
        horizon.validate()?;
        if self.durable.is_none() {
            return self.with_durable_commit_lock(|| {
                let old = self.retention_horizon();
                if old == horizon {
                    return Ok(());
                }
                self.commit_retention_horizon_ledger(&old, &horizon)?;
                self.replace_retention_horizon(horizon)
            });
        }
        // Manifest writes share the checkpoint publisher lock and run outside
        // the global writer lock. A manifest is three atomic-file publications
        // plus readback and generation reclaim; on a cold filesystem that took
        // 6.9 s and parked every unrelated commit when performed inside the
        // global boundary (#1832).
        let durable = self.durable.as_ref().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "durable retention update lost its durable vault handle",
            )
        })?;
        let _checkpoint_guard =
            crate::file_lock::FileLockGuard::acquire(&durable.checkpoint_lock_path())?;
        let old = self.retention_horizon();
        if old == horizon {
            return Ok(());
        }
        durable.write_retention_horizon_manifest(&horizon)?;
        let commit =
            self.with_durable_commit_lock(|| self.commit_retention_horizon_ledger(&old, &horizon));
        if let Err(error) = commit {
            if let Err(rollback) = durable.write_retention_horizon_manifest(&old) {
                tracing::error!(
                    code = "CALYX_ASTER_RETENTION_MANIFEST_ROLLBACK_FAILED",
                    primary_code = error.code,
                    primary_error = %error.message,
                    rollback_code = rollback.code,
                    rollback_error = %rollback.message,
                    "retention horizon ledger publication failed and the off-lock manifest rollback also failed"
                );
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "retention horizon update failed with error[{}]: {}; restoring its manifest also failed with error[{}]: {}",
                    error.code, error.message, rollback.code, rollback.message
                )));
            }
            return Err(error);
        }
        self.replace_retention_horizon(horizon)
    }

    pub(crate) fn replace_retention_horizon(&self, horizon: RetentionHorizon) -> Result<()> {
        *self
            .retention_horizon
            .lock()
            .map_err(|_| retention_lock_error())? = horizon;
        Ok(())
    }

    fn commit_retention_horizon_ledger(
        &self,
        old: &RetentionHorizon,
        new: &RetentionHorizon,
    ) -> Result<()> {
        let payload = serde_json::to_vec(&json!({
            "event": "RETENTION_HORIZON_CHANGED",
            "old": old,
            "new": new,
            "changed_at_millis": self.clock_now(),
        }))
        .map_err(|error| {
            CalyxError::aster_corrupt_shard(format!("encode retention horizon ledger: {error}"))
        })?;
        self.commit_rows_with_ledger_entry_locked(
            Vec::<encode::WriteRow>::new(),
            EntryKind::Admin,
            SubjectId::Guard(RETENTION_HORIZON_SUBJECT.to_vec()),
            payload,
            ActorId::System,
        )?;
        Ok(())
    }
}

fn retention_lock_error() -> CalyxError {
    CalyxError::backpressure("retention horizon lock poisoned")
}
