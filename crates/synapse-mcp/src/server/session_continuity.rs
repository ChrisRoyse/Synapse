use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::ErrorData;
use serde::{Deserialize, Serialize};
use synapse_action::{LeaseOutcome, LeaseStatus, lease};
use synapse_core::error_codes;
use synapse_storage::{Db, cf};

use crate::m3::SharedM3State;

use super::{
    CdpTargetOwner, SessionTarget, SynapseService, m1_tools::validate_target_window, mcp_error,
};

const SESSION_TARGET_PREFIX: &str = "mcp/session-target/v1/";
const SESSION_LEASE_PREFIX: &str = "mcp/session-lease/v1/";
const SESSION_CDP_TARGET_OWNER_PREFIX: &str = "mcp/session-cdp-target-owner/v1/";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionTarget {
    schema_version: u32,
    session_id: String,
    stored_at_unix_ms: u64,
    target: SessionTarget,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionLease {
    schema_version: u32,
    session_id: String,
    stored_at_unix_ms: u64,
    renewed_at_unix_ms: u64,
    ttl_ms: u64,
    expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct PersistedCdpTargetOwner {
    pub schema_version: u32,
    pub owner_key: String,
    pub stored_at_unix_ms: u64,
    pub owner_session_id: String,
    pub owner_client_name: Option<String>,
    pub owner_agent_kind: String,
    pub owner_started_at_unix_ms: Option<u64>,
    pub owner: CdpTargetOwner,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct ContinuityCleanupReadback {
    pub target_row_existed_before: bool,
    pub target_row_exists_after: bool,
    pub target_row_deleted: bool,
    pub lease_row_existed_before: bool,
    pub lease_row_exists_after: bool,
    pub lease_row_deleted: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct LeaseContinuityCleanupReadback {
    pub row_existed_before: bool,
    pub row_exists_after: bool,
    pub row_deleted: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct LeaseHandoffContinuityReadback {
    pub from_row_existed_before: bool,
    pub from_row_exists_after: bool,
    pub from_row_deleted: bool,
    pub to_row_exists_after: bool,
    pub to_row_session_id: Option<String>,
}

impl SynapseService {
    pub(super) fn persist_session_target(
        &self,
        session_id: &str,
        target: &SessionTarget,
    ) -> Result<(), ErrorData> {
        let hwnd = match target {
            SessionTarget::Window { hwnd } => *hwnd,
            SessionTarget::Cdp { window_hwnd, .. } => *window_hwnd,
        };
        crate::m1::validate_window_hwnd_shape("persist_session_target", hwnd)?;
        let row = PersistedSessionTarget {
            schema_version: 1,
            session_id: session_id.to_owned(),
            stored_at_unix_ms: unix_ms_now(),
            target: target.clone(),
        };
        let encoded = synapse_storage::encode_json(&row).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("encode persisted session target failed: {error}"),
            )
        })?;
        let db = self.session_continuity_db()?;
        db.put_batch_pressure_bypass(cf::CF_SESSIONS, [(session_target_key(session_id), encoded)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        tracing::info!(
            code = "MCP_SESSION_TARGET_PERSISTED",
            session_id,
            "persisted active session target to CF_SESSIONS"
        );
        Ok(())
    }

    pub(super) fn delete_persisted_session_target(
        &self,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        let db = self.session_continuity_db()?;
        let key = session_target_key(session_id);
        let existed_before =
            cf_row_exists(&db, &key).map_err(|error| mcp_error(error.code(), error.to_string()))?;
        delete_exact_session_row(&db, key.clone())?;
        let exists_after =
            cf_row_exists(&db, &key).map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if exists_after {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("persisted session target row still exists after delete for {session_id}"),
            ));
        }
        tracing::info!(
            code = "MCP_SESSION_TARGET_DELETED",
            session_id,
            existed_before,
            "deleted active session target from CF_SESSIONS"
        );
        Ok(())
    }

    pub(super) fn delete_persisted_session_target_if_matches(
        &self,
        session_id: &str,
        target: &SessionTarget,
    ) -> Result<bool, ErrorData> {
        let Some(persisted) = self.read_persisted_session_target(session_id)? else {
            return Ok(false);
        };
        if persisted.target != *target {
            tracing::warn!(
                code = "MCP_SESSION_TARGET_DELETE_SKIPPED",
                session_id,
                persisted = ?persisted.target,
                requested = ?target,
                "persisted active session target no longer matches requested cleanup target"
            );
            return Ok(false);
        }
        self.delete_persisted_session_target(session_id)?;
        Ok(true)
    }

    pub(super) fn restore_session_target_if_needed(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        if let Some(target) = self.memory_session_target(session_id)? {
            return Ok(Some(target));
        }
        let Some(persisted) = self.read_persisted_session_target(session_id)? else {
            return Ok(None);
        };
        validate_restored_target(session_id, &persisted.target)?;
        {
            let mut guard = self.session_targets_ref().lock().map_err(|_err| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "session target registry lock poisoned while restoring persisted target",
                )
            })?;
            guard.insert(session_id.to_owned(), persisted.target.clone());
        }
        tracing::info!(
            code = "MCP_SESSION_TARGET_RESTORED",
            session_id,
            stored_at_unix_ms = persisted.stored_at_unix_ms,
            "restored active session target from CF_SESSIONS"
        );
        Ok(Some(persisted.target))
    }

    pub(super) fn persist_cdp_target_owner(
        &self,
        owner_key: &str,
        owner: &CdpTargetOwner,
    ) -> Result<(), ErrorData> {
        crate::m1::validate_window_hwnd_shape("persist_cdp_target_owner", owner.window_hwnd)?;
        if let Some(capture_window_hwnd) = owner.capture_window_hwnd {
            crate::m1::validate_hwnd_shape(
                "persist_cdp_target_owner",
                "capture_window_hwnd",
                capture_window_hwnd,
            )?;
        }
        let owner_read = self.session_registry_read_for_persistence(&owner.session_id)?;
        let row = PersistedCdpTargetOwner {
            schema_version: 1,
            owner_key: owner_key.to_owned(),
            stored_at_unix_ms: unix_ms_now(),
            owner_session_id: owner.session_id.clone(),
            owner_client_name: owner_read
                .as_ref()
                .and_then(|read| read.client_name.clone()),
            owner_agent_kind: owner_read
                .as_ref()
                .map_or_else(|| "unknown".to_owned(), |read| read.agent_kind.clone()),
            owner_started_at_unix_ms: owner_read.as_ref().map(|read| read.started_at_unix_ms),
            owner: owner.clone(),
        };
        let encoded = synapse_storage::encode_json(&row).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("encode persisted CDP target owner failed: {error}"),
            )
        })?;
        let db = self.session_continuity_db()?;
        db.put_batch_pressure_bypass(
            cf::CF_SESSIONS,
            [(
                cdp_target_owner_row_key(owner_key, &owner.cdp_target_id),
                encoded,
            )],
        )
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        tracing::info!(
            code = "MCP_SESSION_CDP_TARGET_OWNER_PERSISTED",
            owner_session_id = %owner.session_id,
            owner_key = %owner_key,
            hwnd = owner.window_hwnd,
            endpoint = %owner.endpoint,
            cdp_target_id = %owner.cdp_target_id,
            "readback=CF_SESSIONS after=cdp_target_owner_persisted"
        );
        Ok(())
    }

    pub(super) fn delete_persisted_cdp_target_owner(
        &self,
        owner_key: &str,
        cdp_target_id: &str,
    ) -> Result<bool, ErrorData> {
        let db = self.session_continuity_db()?;
        delete_persisted_cdp_target_owner_from_db(&db, owner_key, cdp_target_id)
            .map_err(|error| mcp_error(error_codes::STORAGE_CORRUPTED, error))
    }

    pub(super) fn read_persisted_cdp_target_owners_for_target_id(
        &self,
        cdp_target_id: &str,
    ) -> Result<Vec<(String, PersistedCdpTargetOwner)>, ErrorData> {
        let db = self.session_continuity_db()?;
        let prefix = cdp_target_owner_target_prefix(cdp_target_id);
        let rows = db
            .scan_cf_prefix(cf::CF_SESSIONS, &prefix)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let mut decoded = Vec::new();
        for (row_key, value) in rows {
            let row =
                synapse_storage::decode_json::<PersistedCdpTargetOwner>(&value).map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "decode persisted CDP target owner failed for target {cdp_target_id:?}: {error}"
                        ),
                    )
                })?;
            validate_persisted_cdp_target_owner(cdp_target_id, &row)?;
            if row_key != cdp_target_owner_row_key(&row.owner_key, &row.owner.cdp_target_id) {
                return Err(mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!(
                        "persisted CDP target owner row key mismatch for target {cdp_target_id:?}: row_key={} owner_key={}",
                        String::from_utf8_lossy(&row_key),
                        row.owner_key
                    ),
                ));
            }
            decoded.push((row.owner_key.clone(), row));
        }
        decoded.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(decoded)
    }

    pub(super) fn persisted_cdp_target_owner_session_ids(
        &self,
    ) -> Result<BTreeSet<String>, ErrorData> {
        let db = self.session_continuity_db()?;
        read_persisted_cdp_target_owner_session_ids_from_db(&db)
            .map_err(|error| mcp_error(error_codes::STORAGE_CORRUPTED, error))
    }

    pub(super) fn persist_session_lease(
        &self,
        session_id: &str,
        status: &LeaseStatus,
    ) -> Result<(), ErrorData> {
        let ttl_ms = status.ttl_ms.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "cannot persist unheld input lease: missing ttl_ms",
            )
        })?;
        let expires_in_ms = status.expires_in_ms.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "cannot persist unheld input lease: missing expires_in_ms",
            )
        })?;
        if status.owner_session_id.as_deref() != Some(session_id) {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "cannot persist input lease for {session_id}: status owner is {:?}",
                    status.owner_session_id
                ),
            ));
        }
        let now = unix_ms_now();
        let row = PersistedSessionLease {
            schema_version: 1,
            session_id: session_id.to_owned(),
            stored_at_unix_ms: now,
            renewed_at_unix_ms: now.saturating_sub(status.renewed_at_ms_ago.unwrap_or_default()),
            ttl_ms,
            expires_at_unix_ms: now.saturating_add(expires_in_ms),
        };
        let encoded = synapse_storage::encode_json(&row).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("encode persisted session lease failed: {error}"),
            )
        })?;
        let db = self.session_continuity_db()?;
        db.put_batch_pressure_bypass(cf::CF_SESSIONS, [(session_lease_key(session_id), encoded)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        tracing::info!(
            code = "MCP_SESSION_LEASE_PERSISTED",
            session_id,
            ttl_ms,
            expires_at_unix_ms = row.expires_at_unix_ms,
            "persisted active input lease intent to CF_SESSIONS"
        );
        Ok(())
    }

    pub(super) fn persist_session_lease_handoff(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        status: &LeaseStatus,
    ) -> Result<LeaseHandoffContinuityReadback, ErrorData> {
        let ttl_ms = status.ttl_ms.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "cannot persist handoff input lease: missing ttl_ms",
            )
        })?;
        let expires_in_ms = status.expires_in_ms.ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "cannot persist handoff input lease: missing expires_in_ms",
            )
        })?;
        if status.owner_session_id.as_deref() != Some(to_session_id) {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "cannot persist input lease handoff to {to_session_id}: status owner is {:?}",
                    status.owner_session_id
                ),
            ));
        }
        let now = unix_ms_now();
        let row = PersistedSessionLease {
            schema_version: 1,
            session_id: to_session_id.to_owned(),
            stored_at_unix_ms: now,
            renewed_at_unix_ms: now.saturating_sub(status.renewed_at_ms_ago.unwrap_or_default()),
            ttl_ms,
            expires_at_unix_ms: now.saturating_add(expires_in_ms),
        };
        let encoded = synapse_storage::encode_json(&row).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("encode persisted session lease handoff failed: {error}"),
            )
        })?;
        let db = self.session_continuity_db()?;
        let from_key = session_lease_key(from_session_id);
        let to_key = session_lease_key(to_session_id);
        let from_row_existed_before = cf_row_exists(&db, &from_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        db.mutate_batch_pressure_bypass(
            cf::CF_SESSIONS,
            [from_key.clone()],
            [(to_key.clone(), encoded)],
        )
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let from_row_exists_after = cf_row_exists(&db, &from_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let to_row = db
            .scan_cf_prefix(cf::CF_SESSIONS, &to_key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .into_iter()
            .find(|(row_key, _value)| row_key == &to_key);
        let to_row_exists_after = to_row.is_some();
        let to_row_session_id = to_row
            .map(|(_key, value)| {
                synapse_storage::decode_json::<PersistedSessionLease>(&value)
                    .map(|lease| lease.session_id)
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "decode persisted session lease handoff failed for {to_session_id}: {error}"
                            ),
                        )
                    })
            })
            .transpose()?;
        if from_row_exists_after
            || !to_row_exists_after
            || to_row_session_id.as_deref() != Some(to_session_id)
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "persisted session lease handoff readback mismatch from={from_session_id} to={to_session_id}: from_after={from_row_exists_after} to_after={to_row_exists_after} to_row_session_id={to_row_session_id:?}"
                ),
            ));
        }
        let readback = LeaseHandoffContinuityReadback {
            from_row_existed_before,
            from_row_exists_after,
            from_row_deleted: from_row_existed_before && !from_row_exists_after,
            to_row_exists_after,
            to_row_session_id,
        };
        tracing::info!(
            code = "MCP_SESSION_LEASE_HANDOFF_PERSISTED",
            from_session_id,
            to_session_id,
            readback = ?readback,
            ttl_ms,
            expires_at_unix_ms = row.expires_at_unix_ms,
            "readback=CF_SESSIONS after=session_lease_handoff_persisted"
        );
        Ok(readback)
    }

    pub(super) fn delete_persisted_session_lease(&self, session_id: &str) -> Result<(), ErrorData> {
        let db = self.session_continuity_db()?;
        let key = session_lease_key(session_id);
        let existed_before =
            cf_row_exists(&db, &key).map_err(|error| mcp_error(error.code(), error.to_string()))?;
        delete_exact_session_row(&db, key.clone())?;
        let exists_after =
            cf_row_exists(&db, &key).map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if exists_after {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("persisted session lease row still exists after delete for {session_id}"),
            ));
        }
        tracing::info!(
            code = "MCP_SESSION_LEASE_DELETED",
            session_id,
            existed_before,
            "deleted input lease intent from CF_SESSIONS"
        );
        Ok(())
    }

    pub(super) fn restore_session_lease_if_needed(
        &self,
        session_id: &str,
    ) -> Result<(), ErrorData> {
        let Some(persisted) = self.read_persisted_session_lease(session_id)? else {
            return Ok(());
        };
        let now = unix_ms_now();
        if persisted.expires_at_unix_ms <= now {
            self.delete_persisted_session_lease(session_id)?;
            tracing::warn!(
                code = "MCP_SESSION_LEASE_EXPIRED_DELETE",
                session_id,
                expires_at_unix_ms = persisted.expires_at_unix_ms,
                now_unix_ms = now,
                "deleted expired persisted input lease intent"
            );
            return Ok(());
        }
        let current = lease::status();
        if current.held {
            tracing::info!(
                code = "MCP_SESSION_LEASE_RESTORE_SKIPPED",
                session_id,
                current_owner = ?current.owner_session_id,
                "input lease already held; persisted intent was not restored"
            );
            return Ok(());
        }
        let remaining_ms = persisted.expires_at_unix_ms.saturating_sub(now);
        if remaining_ms < synapse_action::MIN_LEASE_TTL_MS {
            self.delete_persisted_session_lease(session_id)?;
            tracing::warn!(
                code = "MCP_SESSION_LEASE_TOO_CLOSE_TO_EXPIRY_DELETE",
                session_id,
                remaining_ms,
                min_restore_ttl_ms = synapse_action::MIN_LEASE_TTL_MS,
                "deleted persisted input lease intent that was too close to expiry to restore without extending it"
            );
            return Ok(());
        }
        match lease::try_acquire(session_id, lease::ttl_from_ms(remaining_ms)) {
            LeaseOutcome::Acquired(status) | LeaseOutcome::Renewed(status) => {
                if let Err(error) = self.persist_session_lease(session_id, &status) {
                    let released = lease::release_if_owner(session_id);
                    tracing::error!(
                        code = error_codes::TOOL_INTERNAL_ERROR,
                        session_id,
                        released_after_persist_failure = released,
                        error = ?error,
                        "input lease restore failed durability write; released in-memory lease before returning error"
                    );
                    return Err(error);
                }
                tracing::info!(
                    code = "MCP_SESSION_LEASE_RESTORED",
                    session_id,
                    restored_expires_in_ms = status.expires_in_ms,
                    "restored input lease from CF_SESSIONS"
                );
                Ok(())
            }
            LeaseOutcome::Busy {
                holder,
                retry_after_ms,
            } => {
                tracing::warn!(
                    code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                    session_id,
                    holder = ?holder.owner_session_id,
                    retry_after_ms,
                    "persisted input lease intent could not restore because the lease is contended"
                );
                Ok(())
            }
            LeaseOutcome::CleanupPending {
                expired,
                retry_after_ms,
            } => {
                tracing::warn!(
                    code = error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                    session_id,
                    expired_owner = ?expired.owner_session_id,
                    retry_after_ms,
                    "persisted input lease intent could not restore because expired input cleanup is pending"
                );
                Ok(())
            }
        }
    }

    pub(super) fn memory_session_target(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        let guard = self.session_targets_ref().lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session target registry lock poisoned",
            )
        })?;
        Ok(guard.get(session_id).cloned())
    }

    pub(super) fn persisted_session_target_read_model(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        Ok(self
            .read_persisted_session_target(session_id)?
            .map(|persisted| persisted.target))
    }

    fn read_persisted_session_target(
        &self,
        session_id: &str,
    ) -> Result<Option<PersistedSessionTarget>, ErrorData> {
        let key = session_target_key(session_id);
        let db = self.session_continuity_db()?;
        let rows = db
            .scan_cf_prefix(cf::CF_SESSIONS, &key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let Some((_row_key, value)) = rows.into_iter().find(|(row_key, _)| row_key == &key) else {
            return Ok(None);
        };
        let persisted =
            synapse_storage::decode_json::<PersistedSessionTarget>(&value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("decode persisted session target failed for {session_id}: {error}"),
                )
            })?;
        if persisted.schema_version != 1 || persisted.session_id != session_id {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "persisted session target row mismatch for {session_id}: schema_version={} row_session_id={}",
                    persisted.schema_version, persisted.session_id
                ),
            ));
        }
        validate_persisted_session_target_hwnd(session_id, &persisted.target)?;
        Ok(Some(persisted))
    }

    fn read_persisted_session_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<PersistedSessionLease>, ErrorData> {
        let key = session_lease_key(session_id);
        let db = self.session_continuity_db()?;
        let rows = db
            .scan_cf_prefix(cf::CF_SESSIONS, &key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let Some((_row_key, value)) = rows.into_iter().find(|(row_key, _)| row_key == &key) else {
            return Ok(None);
        };
        let persisted =
            synapse_storage::decode_json::<PersistedSessionLease>(&value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("decode persisted session lease failed for {session_id}: {error}"),
                )
            })?;
        if persisted.schema_version != 1 || persisted.session_id != session_id {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "persisted session lease row mismatch for {session_id}: schema_version={} row_session_id={}",
                    persisted.schema_version, persisted.session_id
                ),
            ));
        }
        Ok(Some(persisted))
    }

    fn session_registry_read_for_persistence(
        &self,
        session_id: &str,
    ) -> Result<Option<super::session_registry::SessionRegistryRead>, ErrorData> {
        let now = unix_ms_now();
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while reading CDP owner client identity",
            )
        })?;
        Ok(guard
            .reads(now)
            .into_iter()
            .find(|read| read.session_id == session_id))
    }

    fn session_continuity_db(&self) -> Result<Arc<Db>, ErrorData> {
        let mut state = self.m3_state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening session continuity storage",
            )
        })?;
        state
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }
}

pub(crate) fn delete_persisted_session_continuity_rows(
    m3_state: &SharedM3State,
    session_id: &str,
) -> Result<ContinuityCleanupReadback, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    delete_persisted_session_continuity_rows_from_db(&db, session_id)
}

pub(crate) fn read_persisted_session_target_for_session(
    m3_state: &SharedM3State,
    session_id: &str,
) -> Result<Option<SessionTarget>, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    read_persisted_session_target_from_db(&db, session_id).map(|row| row.map(|row| row.target))
}

pub(crate) fn delete_persisted_session_continuity_rows_from_db(
    db: &Db,
    session_id: &str,
) -> Result<ContinuityCleanupReadback, String> {
    let target_key = session_target_key(session_id);
    let lease_key = session_lease_key(session_id);
    let target_row_existed_before =
        cf_row_exists(db, &target_key).map_err(|error| error.to_string())?;
    let lease_row_existed_before =
        cf_row_exists(db, &lease_key).map_err(|error| error.to_string())?;
    db.delete_batch(cf::CF_SESSIONS, [target_key.clone(), lease_key.clone()])
        .map_err(|error| error.to_string())?;
    let target_row_exists_after =
        cf_row_exists(db, &target_key).map_err(|error| error.to_string())?;
    let lease_row_exists_after =
        cf_row_exists(db, &lease_key).map_err(|error| error.to_string())?;
    if target_row_exists_after || lease_row_exists_after {
        return Err(format!(
            "session continuity rows still exist after delete for {session_id}: target_after={target_row_exists_after} lease_after={lease_row_exists_after}"
        ));
    }
    let readback = ContinuityCleanupReadback {
        target_row_existed_before,
        target_row_exists_after,
        target_row_deleted: target_row_existed_before && !target_row_exists_after,
        lease_row_existed_before,
        lease_row_exists_after,
        lease_row_deleted: lease_row_existed_before && !lease_row_exists_after,
    };
    tracing::info!(
        code = "MCP_SESSION_CONTINUITY_DELETED",
        session_id,
        readback = ?readback,
        "readback=CF_SESSIONS after=session_continuity_deleted"
    );
    Ok(readback)
}

fn read_persisted_session_target_from_db(
    db: &Db,
    session_id: &str,
) -> Result<Option<PersistedSessionTarget>, String> {
    let key = session_target_key(session_id);
    let rows = db
        .scan_cf_prefix(cf::CF_SESSIONS, &key)
        .map_err(|error| error.to_string())?;
    let Some((_row_key, value)) = rows.into_iter().find(|(row_key, _)| row_key == &key) else {
        return Ok(None);
    };
    let persisted =
        synapse_storage::decode_json::<PersistedSessionTarget>(&value).map_err(|error| {
            format!("decode persisted session target failed for {session_id}: {error}")
        })?;
    if persisted.schema_version != 1 || persisted.session_id != session_id {
        return Err(format!(
            "persisted session target row mismatch for {session_id}: schema_version={} row_session_id={}",
            persisted.schema_version, persisted.session_id
        ));
    }
    validate_persisted_session_target_hwnd(session_id, &persisted.target)
        .map_err(|error| error.message.to_string())?;
    Ok(Some(persisted))
}

pub(crate) fn delete_persisted_session_lease_row(
    m3_state: &SharedM3State,
    session_id: &str,
) -> Result<LeaseContinuityCleanupReadback, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    delete_persisted_session_lease_row_from_db(&db, session_id)
}

/// Exact durable-lease row snapshot used by cancellation-safe authority
/// guards. Keeping the encoded bytes lets a guard restore the pre-call row
/// without reconstructing or extending its expiry metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedSessionLeaseRowSnapshot {
    value: Option<Vec<u8>>,
}

impl PersistedSessionLeaseRowSnapshot {
    pub(crate) const fn row_exists(&self) -> bool {
        self.value.is_some()
    }
}

pub(crate) fn snapshot_persisted_session_lease_row(
    m3_state: &SharedM3State,
    session_id: &str,
) -> Result<PersistedSessionLeaseRowSnapshot, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    snapshot_persisted_session_lease_row_from_db(&db, session_id)
}

fn snapshot_persisted_session_lease_row_from_db(
    db: &Db,
    session_id: &str,
) -> Result<PersistedSessionLeaseRowSnapshot, String> {
    let key = session_lease_key(session_id);
    let value = db
        .scan_cf_prefix(cf::CF_SESSIONS, &key)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find_map(|(row_key, value)| (row_key == key).then_some(value));
    Ok(PersistedSessionLeaseRowSnapshot { value })
}

/// Restore an exact pre-call durable-lease row and immediately read it back.
/// This is synchronous by design so an async future's `Drop` path can revoke a
/// partially acquired foreground lease even during cancellation or unwind.
pub(crate) fn restore_persisted_session_lease_row(
    m3_state: &SharedM3State,
    session_id: &str,
    before: &PersistedSessionLeaseRowSnapshot,
) -> Result<PersistedSessionLeaseRowSnapshot, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    let key = session_lease_key(session_id);
    match before.value.as_ref() {
        Some(value) => db
            .put_batch_pressure_bypass(cf::CF_SESSIONS, [(key, value.clone())])
            .map_err(|error| error.to_string())?,
        None => db
            .delete_batch(cf::CF_SESSIONS, [key])
            .map_err(|error| error.to_string())?,
    }
    let after = snapshot_persisted_session_lease_row_from_db(&db, session_id)?;
    if &after != before {
        return Err(format!(
            "persisted session lease row mismatch after authority-guard restore for {session_id}: expected_present={} actual_present={}",
            before.value.is_some(),
            after.value.is_some()
        ));
    }
    tracing::info!(
        code = "MCP_SESSION_LEASE_AUTHORITY_GUARD_RESTORED",
        session_id,
        row_present = after.value.is_some(),
        "readback=CF_SESSIONS after=authority_guard_lease_row_restored"
    );
    Ok(after)
}

pub(crate) fn delete_persisted_session_lease_row_from_db(
    db: &Db,
    session_id: &str,
) -> Result<LeaseContinuityCleanupReadback, String> {
    let key = session_lease_key(session_id);
    let row_existed_before = cf_row_exists(db, &key).map_err(|error| error.to_string())?;
    db.delete_batch(cf::CF_SESSIONS, [key.clone()])
        .map_err(|error| error.to_string())?;
    let row_exists_after = cf_row_exists(db, &key).map_err(|error| error.to_string())?;
    if row_exists_after {
        return Err(format!(
            "session lease continuity row still exists after delete for {session_id}"
        ));
    }
    let readback = LeaseContinuityCleanupReadback {
        row_existed_before,
        row_exists_after,
        row_deleted: row_existed_before && !row_exists_after,
    };
    tracing::info!(
        code = "MCP_SESSION_LEASE_CONTINUITY_DELETED",
        session_id,
        readback = ?readback,
        "readback=CF_SESSIONS after=session_lease_continuity_deleted"
    );
    Ok(readback)
}

fn validate_restored_target(session_id: &str, target: &SessionTarget) -> Result<(), ErrorData> {
    match target {
        SessionTarget::Window { hwnd } => validate_target_window(*hwnd).map(|_| ()),
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => {
            if cdp_target_id.trim().is_empty() {
                return Err(mcp_error(
                    error_codes::TARGET_CDP_UNRESOLVED,
                    format!(
                        "persisted CDP session target for {session_id} has an empty cdp_target_id"
                    ),
                ));
            }
            validate_target_window(*window_hwnd).map(|_| ())
        }
    }
}

fn delete_exact_session_row(db: &Db, key: Vec<u8>) -> Result<(), ErrorData> {
    db.delete_batch(cf::CF_SESSIONS, [key])
        .map_err(|error| mcp_error(error.code(), error.to_string()))
}

fn cf_row_exists(db: &Db, key: &[u8]) -> synapse_storage::StorageResult<bool> {
    db.scan_cf_prefix(cf::CF_SESSIONS, key).map(|rows| {
        rows.into_iter()
            .any(|(row_key, _value)| row_key.as_slice() == key)
    })
}

pub(crate) fn delete_persisted_cdp_target_owner_row(
    m3_state: &SharedM3State,
    owner_key: &str,
    cdp_target_id: &str,
) -> Result<bool, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    delete_persisted_cdp_target_owner_from_db(&db, owner_key, cdp_target_id)
}

pub(crate) fn read_persisted_cdp_target_owners_for_session(
    m3_state: &SharedM3State,
    session_id: &str,
) -> Result<Vec<(String, PersistedCdpTargetOwner)>, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    read_persisted_cdp_target_owners_for_session_from_db(&db, session_id)
}

pub(crate) fn persisted_session_target_session_ids(
    m3_state: &SharedM3State,
) -> Result<BTreeSet<String>, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    read_persisted_session_target_session_ids_from_db(&db)
}

pub(crate) fn persisted_cdp_target_owner_session_ids(
    m3_state: &SharedM3State,
) -> Result<BTreeSet<String>, String> {
    let db = session_continuity_db_from_state(m3_state)?;
    read_persisted_cdp_target_owner_session_ids_from_db(&db)
}

pub(crate) fn persisted_cdp_target_owner_row_key_string(
    owner_key: &str,
    cdp_target_id: &str,
) -> String {
    String::from_utf8_lossy(&cdp_target_owner_row_key(owner_key, cdp_target_id)).into_owned()
}

fn delete_persisted_cdp_target_owner_from_db(
    db: &Db,
    owner_key: &str,
    cdp_target_id: &str,
) -> Result<bool, String> {
    let key = cdp_target_owner_row_key(owner_key, cdp_target_id);
    let existed_before = cf_row_exists(db, &key).map_err(|error| error.to_string())?;
    db.delete_batch(cf::CF_SESSIONS, [key.clone()])
        .map_err(|error| error.to_string())?;
    let exists_after = cf_row_exists(db, &key).map_err(|error| error.to_string())?;
    if exists_after {
        return Err(format!(
            "persisted CDP target owner row still exists after delete owner_key={owner_key:?} cdp_target_id={cdp_target_id:?}"
        ));
    }
    tracing::info!(
        code = "MCP_SESSION_CDP_TARGET_OWNER_DELETED",
        owner_key = %owner_key,
        cdp_target_id = %cdp_target_id,
        existed_before,
        "readback=CF_SESSIONS after=cdp_target_owner_deleted"
    );
    Ok(existed_before)
}

fn read_persisted_cdp_target_owners_for_session_from_db(
    db: &Db,
    session_id: &str,
) -> Result<Vec<(String, PersistedCdpTargetOwner)>, String> {
    let rows = db
        .scan_cf_prefix(cf::CF_SESSIONS, SESSION_CDP_TARGET_OWNER_PREFIX.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut decoded = Vec::new();
    for (row_key, value) in rows {
        let row = synapse_storage::decode_json::<PersistedCdpTargetOwner>(&value)
            .map_err(|error| format!("decode persisted CDP target owner failed: {error}"))?;
        validate_persisted_cdp_target_owner(&row.owner.cdp_target_id, &row)
            .map_err(|error| error.message.to_string())?;
        let expected_key = cdp_target_owner_row_key(&row.owner_key, &row.owner.cdp_target_id);
        if row_key != expected_key {
            return Err(format!(
                "persisted CDP target owner row key mismatch: row_key={} owner_key={}",
                String::from_utf8_lossy(&row_key),
                row.owner_key
            ));
        }
        if row.owner_session_id == session_id {
            decoded.push((row.owner_key.clone(), row));
        }
    }
    decoded.sort_by(|left, right| {
        left.1
            .owner
            .cdp_target_id
            .cmp(&right.1.owner.cdp_target_id)
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(decoded)
}

fn read_persisted_session_target_session_ids_from_db(db: &Db) -> Result<BTreeSet<String>, String> {
    let rows = db
        .scan_cf_prefix(cf::CF_SESSIONS, SESSION_TARGET_PREFIX.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut session_ids = BTreeSet::new();
    for (row_key, value) in rows {
        let key_session_id = session_id_from_target_row_key(&row_key)?;
        let row = synapse_storage::decode_json::<PersistedSessionTarget>(&value)
            .map_err(|error| format!("decode persisted session target failed: {error}"))?;
        if row.schema_version != 1 || row.session_id != key_session_id {
            return Err(format!(
                "persisted session target row mismatch: row_key={} schema_version={} row_session_id={}",
                String::from_utf8_lossy(&row_key),
                row.schema_version,
                row.session_id
            ));
        }
        validate_persisted_session_target_hwnd(&key_session_id, &row.target)
            .map_err(|error| error.message.to_string())?;
        session_ids.insert(key_session_id);
    }
    Ok(session_ids)
}

fn read_persisted_cdp_target_owner_session_ids_from_db(
    db: &Db,
) -> Result<BTreeSet<String>, String> {
    let rows = db
        .scan_cf_prefix(cf::CF_SESSIONS, SESSION_CDP_TARGET_OWNER_PREFIX.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut session_ids = BTreeSet::new();
    for (row_key, value) in rows {
        let row = synapse_storage::decode_json::<PersistedCdpTargetOwner>(&value)
            .map_err(|error| format!("decode persisted CDP target owner failed: {error}"))?;
        validate_persisted_cdp_target_owner(&row.owner.cdp_target_id, &row)
            .map_err(|error| error.message.to_string())?;
        let expected_key = cdp_target_owner_row_key(&row.owner_key, &row.owner.cdp_target_id);
        if row_key != expected_key {
            return Err(format!(
                "persisted CDP target owner row key mismatch: row_key={} owner_key={}",
                String::from_utf8_lossy(&row_key),
                row.owner_key
            ));
        }
        session_ids.insert(row.owner_session_id);
    }
    Ok(session_ids)
}

fn session_id_from_target_row_key(row_key: &[u8]) -> Result<String, String> {
    let key = std::str::from_utf8(row_key)
        .map_err(|error| format!("persisted session target row key is not UTF-8: {error}"))?;
    let Some(session_id) = key.strip_prefix(SESSION_TARGET_PREFIX) else {
        return Err(format!(
            "persisted session target row key has unexpected prefix: {key}"
        ));
    };
    if session_id.is_empty()
        || session_id.chars().count() > 512
        || !session_id.chars().all(|ch| ('!'..='~').contains(&ch))
    {
        return Err(format!(
            "persisted session target row key has invalid session id: {key}"
        ));
    }
    Ok(session_id.to_owned())
}

fn validate_persisted_cdp_target_owner(
    requested_cdp_target_id: &str,
    row: &PersistedCdpTargetOwner,
) -> Result<(), ErrorData> {
    if row.schema_version != 1 {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "persisted CDP target owner row has unsupported schema_version={}",
                row.schema_version
            ),
        ));
    }
    if row.owner_key.trim().is_empty() {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            "persisted CDP target owner row has empty owner_key",
        ));
    }
    if row.owner_session_id != row.owner.session_id {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "persisted CDP target owner session mismatch: row_session_id={} owner_session_id={}",
                row.owner_session_id, row.owner.session_id
            ),
        ));
    }
    if !crate::m1::window_hwnd_shape_is_canonical(row.owner.window_hwnd) {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "persisted CDP target owner row has noncanonical window_hwnd={}; expected 1..=4294967295",
                row.owner.window_hwnd
            ),
        ));
    }
    if let Some(capture_window_hwnd) = row.owner.capture_window_hwnd
        && !crate::m1::window_hwnd_shape_is_canonical(capture_window_hwnd)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "persisted CDP target owner row has noncanonical capture_window_hwnd={capture_window_hwnd}; expected 1..=4294967295"
            ),
        ));
    }
    if normalize_cdp_target_id(&row.owner.cdp_target_id)
        != normalize_cdp_target_id(requested_cdp_target_id)
    {
        return Err(mcp_error(
            error_codes::STORAGE_CORRUPTED,
            format!(
                "persisted CDP target owner target mismatch: requested={requested_cdp_target_id:?} row={:?}",
                row.owner.cdp_target_id
            ),
        ));
    }
    Ok(())
}

fn validate_persisted_session_target_hwnd(
    session_id: &str,
    target: &SessionTarget,
) -> Result<(), ErrorData> {
    let (field, hwnd) = match target {
        SessionTarget::Window { hwnd } => ("hwnd", *hwnd),
        SessionTarget::Cdp { window_hwnd, .. } => ("window_hwnd", *window_hwnd),
    };
    if crate::m1::window_hwnd_shape_is_canonical(hwnd) {
        return Ok(());
    }
    tracing::error!(
        code = error_codes::STORAGE_CORRUPTED,
        source_of_truth = "CF_SESSIONS persisted session target row",
        session_id,
        field,
        actual_value = hwnd,
        accepted_range = "1..=u32::MAX",
        remediation = "delete or repair the corrupt session-target row, then bind a canonical target through the target facade",
        "persisted session target contains a noncanonical HWND"
    );
    Err(mcp_error(
        error_codes::STORAGE_CORRUPTED,
        format!(
            "persisted session target row for {session_id} has noncanonical {field}={hwnd}; expected 1..=4294967295"
        ),
    ))
}

fn cdp_target_owner_target_prefix(cdp_target_id: &str) -> Vec<u8> {
    let normalized = normalize_cdp_target_id(cdp_target_id);
    format!(
        "{SESSION_CDP_TARGET_OWNER_PREFIX}{}:{normalized}/",
        normalized.len()
    )
    .into_bytes()
}

fn cdp_target_owner_row_key(owner_key: &str, cdp_target_id: &str) -> Vec<u8> {
    format!(
        "{}{}",
        String::from_utf8_lossy(&cdp_target_owner_target_prefix(cdp_target_id)),
        owner_key
    )
    .into_bytes()
}

fn normalize_cdp_target_id(cdp_target_id: &str) -> String {
    cdp_target_id.trim().to_ascii_lowercase()
}

fn session_continuity_db_from_state(m3_state: &SharedM3State) -> Result<Arc<Db>, String> {
    let mut state = m3_state.lock().map_err(|_error| {
        "M3 service state lock poisoned while opening session continuity storage".to_owned()
    })?;
    state
        .ensure_storage()
        .map_err(|error| format!("open storage for session continuity cleanup: {error}"))
}

fn session_target_key(session_id: &str) -> Vec<u8> {
    format!("{SESSION_TARGET_PREFIX}{session_id}").into_bytes()
}

fn session_lease_key(session_id: &str) -> Vec<u8> {
    format!("{SESSION_LEASE_PREFIX}{session_id}").into_bytes()
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
