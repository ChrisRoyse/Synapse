use super::{AsterVault, encode, ledger_hook};
use crate::cf::{ColumnFamily, base_key};
use calyx_core::{
    CalyxError, Clock, CxId, LedgerRef, METADATA_SOURCE_EVENT_TIME_RAW,
    METADATA_SOURCE_EVENT_TIME_SECS, METADATA_SOURCE_SEQUENCE, METADATA_TEMPORAL_INACTIVE_REASON,
    METADATA_TEMPORAL_LANE_STATE, Result, TEMPORAL_LANE_ACTIVE, VaultStore,
};
use calyx_ledger::{ActorId, EntryKind, RedactionPolicy, SubjectId};
use std::collections::BTreeMap;

pub const CALYX_TEMPORAL_METADATA_MIGRATION_MISMATCH: &str =
    "CALYX_TEMPORAL_METADATA_MIGRATION_MISMATCH";

const TEMPORAL_KEYS: [&str; 5] = [
    METADATA_TEMPORAL_LANE_STATE,
    METADATA_TEMPORAL_INACTIVE_REASON,
    METADATA_SOURCE_EVENT_TIME_SECS,
    METADATA_SOURCE_EVENT_TIME_RAW,
    METADATA_SOURCE_SEQUENCE,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemporalMetadataMigration {
    AlreadyPresent { ledger_ref: LedgerRef },
    Backfilled { ledger_ref: LedgerRef },
}

impl TemporalMetadataMigration {
    pub fn changed(&self) -> bool {
        matches!(self, Self::Backfilled { .. })
    }

    pub fn ledger_ref(&self) -> &LedgerRef {
        match self {
            Self::AlreadyPresent { ledger_ref } | Self::Backfilled { ledger_ref } => ledger_ref,
        }
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Reconstructs temporal metadata on one legacy Base row from an exact,
    /// authoritative constellation rebuilt by the owning application.
    pub fn backfill_temporal_metadata(
        &self,
        id: CxId,
        expected_panel_version: u32,
        expected_identity: &BTreeMap<String, String>,
        expected_temporal: &BTreeMap<String, String>,
    ) -> Result<TemporalMetadataMigration> {
        validate_temporal_metadata(id, expected_temporal)?;
        self.with_durable_commit_lock(|| {
            let snapshot = self.snapshot_handle(self.snapshot());
            let mut stored = self.get_base_at_snapshot(id, snapshot.snapshot())?;
            if stored.panel_version != expected_panel_version {
                return Err(mismatch(
                    id,
                    &format!(
                        "stored panel version {} differs from authoritative version {expected_panel_version}",
                        stored.panel_version
                    ),
                ));
            }
            for (key, expected) in expected_identity {
                match stored.metadata.get(key) {
                    Some(actual) if actual == expected => {}
                    Some(actual) => {
                        return Err(mismatch(
                            id,
                            &format!(
                                "stored identity metadata {key}={actual:?} differs from authoritative value {expected:?}"
                            ),
                        ));
                    }
                    None => {
                        return Err(mismatch(
                            id,
                            &format!("stored identity metadata {key} is missing"),
                        ));
                    }
                }
            }

            let already_present = TEMPORAL_KEYS.iter().all(|key| {
                stored.metadata.get(*key) == expected_temporal.get(*key)
            });
            if already_present {
                return Ok(TemporalMetadataMigration::AlreadyPresent {
                    ledger_ref: stored.provenance,
                });
            }

            for key in TEMPORAL_KEYS {
                match expected_temporal.get(key) {
                    Some(value) => {
                        stored.metadata.insert(key.to_owned(), value.clone());
                    }
                    None => {
                        stored.metadata.remove(key);
                    }
                }
            }

            let payload = migration_payload(id, expected_panel_version, expected_temporal)?;
            let actor = ActorId::Service("calyx-aster".to_string());
            let subject = SubjectId::Cx(id);
            let mut rows = Vec::new();
            let mut hook_guard = match &self.ledger_hook {
                Some(hook) => Some(ledger_hook::lock_hook(hook)?),
                None => None,
            };
            let staged = if let Some(hook) = hook_guard.as_deref() {
                let staged = ledger_hook::stage_entry_payload(
                    hook,
                    &mut rows,
                    EntryKind::Migrate,
                    subject,
                    payload,
                    actor,
                )?;
                let ledger_ref = staged
                    .first()
                    .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                    .ledger_ref();
                Some((staged, ledger_ref))
            } else {
                let ledger_ref = self.stage_raw_ledger_entry_locked(
                    &mut rows,
                    EntryKind::Migrate,
                    subject,
                    payload,
                    actor,
                )?;
                stored.provenance = ledger_ref.clone();
                stored.validate_schema()?;
                rows.push(base_row(id, &stored)?);
                self.commit_rows_locked(&rows)?;
                return Ok(TemporalMetadataMigration::Backfilled { ledger_ref });
            };

            let (staged, ledger_ref) = staged.expect("hook branch returns staged rows");
            stored.provenance = ledger_ref.clone();
            stored.validate_schema()?;
            rows.push(base_row(id, &stored)?);
            self.commit_rows_locked(&rows)?;
            if let Some(hook) = hook_guard.take() {
                self.commit_persistent_ledger_staged_locked(
                    hook,
                    &staged,
                    "temporal_metadata_backfill",
                )?;
            }
            Ok(TemporalMetadataMigration::Backfilled { ledger_ref })
        })
    }
}

fn validate_temporal_metadata(
    id: CxId,
    expected_temporal: &BTreeMap<String, String>,
) -> Result<()> {
    if expected_temporal
        .keys()
        .any(|key| !TEMPORAL_KEYS.contains(&key.as_str()))
    {
        return Err(mismatch(
            id,
            "incoming map contains a non-temporal metadata key",
        ));
    }
    if expected_temporal
        .get(METADATA_TEMPORAL_LANE_STATE)
        .map(String::as_str)
        != Some(TEMPORAL_LANE_ACTIVE)
    {
        return Err(mismatch(
            id,
            "authoritative temporal metadata does not declare an active lane",
        ));
    }
    for key in [
        METADATA_SOURCE_EVENT_TIME_SECS,
        METADATA_SOURCE_EVENT_TIME_RAW,
        METADATA_SOURCE_SEQUENCE,
    ] {
        if expected_temporal.get(key).is_none_or(String::is_empty) {
            return Err(mismatch(
                id,
                &format!("authoritative temporal metadata {key} is missing or empty"),
            ));
        }
    }
    Ok(())
}

fn base_row(id: CxId, stored: &calyx_core::Constellation) -> Result<encode::WriteRow> {
    Ok(encode::WriteRow {
        cf: ColumnFamily::Base,
        key: base_key(id),
        value: encode::encode_constellation_base(stored)?,
    })
}

fn migration_payload(
    id: CxId,
    panel_version: u32,
    temporal: &BTreeMap<String, String>,
) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "mode": "temporal-metadata-backfill",
        "cx_id": id.to_string(),
        "panel_version": panel_version,
        "temporal_metadata": temporal,
    }))
    .map_err(|error| {
        CalyxError::ledger_group_commit_failed(format!(
            "encode temporal metadata backfill payload: {error}"
        ))
    })?;
    RedactionPolicy::check_payload(&payload)?;
    Ok(payload)
}

fn mismatch(id: CxId, reason: &str) -> CalyxError {
    CalyxError {
        code: CALYX_TEMPORAL_METADATA_MIGRATION_MISMATCH,
        message: format!("cannot backfill temporal metadata for {id}: {reason}"),
        remediation: "rebuild the candidate from its exact authoritative source row and registered panel; never infer temporal metadata from legacy slot numbers",
    }
}
