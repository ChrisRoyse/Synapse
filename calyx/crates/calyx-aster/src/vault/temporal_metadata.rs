use super::base_rewrite::BaseRowRewrite;
use super::{AsterVault, encode, ledger_hook};
use crate::cf::{ColumnFamily, base_key};
use calyx_core::{
    CalyxError, Clock, CxId, LedgerRef, METADATA_SOURCE_EVENT_TIME_RAW,
    METADATA_SOURCE_EVENT_TIME_SECS, METADATA_SOURCE_SEQUENCE, METADATA_TEMPORAL_INACTIVE_REASON,
    METADATA_TEMPORAL_LANE_STATE, Result, TEMPORAL_LANE_ACTIVE, VaultStore,
};
use calyx_ledger::{ActorId, EntryKind, RedactionPolicy, SubjectId, declare_batch_members};
use std::collections::{BTreeMap, BTreeSet};

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

/// One authoritative temporal-metadata reconstruction request.
///
/// The owning application must rebuild both maps from the exact source row and
/// registered panel. Aster verifies identity fields against the stored Base row
/// before changing only the temporal keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemporalMetadataBackfill {
    pub cx_id: CxId,
    pub expected_panel_version: u32,
    pub expected_identity: BTreeMap<String, String>,
    pub expected_temporal: BTreeMap<String, String>,
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
        let request = TemporalMetadataBackfill {
            cx_id: id,
            expected_panel_version,
            expected_identity: expected_identity.clone(),
            expected_temporal: expected_temporal.clone(),
        };
        let mut migrations = self.backfill_temporal_metadata_batch([request])?;
        migrations.pop().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "one-row temporal metadata backfill returned no migration outcome",
            )
        })
    }

    /// Reconstructs temporal metadata for an ordered set of legacy Base rows
    /// under one durable commit boundary.
    ///
    /// Validation and snapshot reads happen for the complete input before any
    /// mutation. Already-current rows retain their existing provenance; changed
    /// rows share one declared batch ledger entry and one MVCC/WAL commit. The
    /// returned outcomes remain in input order.
    pub fn backfill_temporal_metadata_batch<I>(
        &self,
        requests: I,
    ) -> Result<Vec<TemporalMetadataMigration>>
    where
        I: IntoIterator<Item = TemporalMetadataBackfill>,
    {
        let input = requests.into_iter().collect::<Vec<_>>();
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique_ids = BTreeSet::new();
        for request in &input {
            validate_temporal_metadata(request.cx_id, &request.expected_temporal)?;
            if !unique_ids.insert(request.cx_id) {
                return Err(mismatch(
                    request.cx_id,
                    "the same Base identity appears more than once in one temporal metadata batch",
                ));
            }
        }
        self.with_durable_commit_lock(|| {
            let snapshot = self.snapshot_handle(self.snapshot());
            let mut outcomes = vec![None; input.len()];
            let mut pending = Vec::new();
            for (input_index, request) in input.iter().enumerate() {
                let id = request.cx_id;
                // Rewrites the stored Base row in place, carrying its slot
                // hashes. `get_base_at_snapshot` clears `slots`, so re-encoding
                // from it would orphan every Slot-CF row (issue #1888).
                let base_bytes = self
                    .read_cf_snapshot(snapshot.snapshot(), ColumnFamily::Base, &base_key(id))?
                    .ok_or_else(|| mismatch(id, "Base row is missing"))?;
                let mut rewrite = BaseRowRewrite::decode(&base_bytes)?;
                let stored = rewrite.constellation();
                if stored.panel_version != request.expected_panel_version {
                    return Err(mismatch(
                        id,
                        &format!(
                            "stored panel version {} differs from authoritative version {}",
                            stored.panel_version, request.expected_panel_version
                        ),
                    ));
                }
                for (key, expected) in &request.expected_identity {
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
                    stored.metadata.get(*key) == request.expected_temporal.get(*key)
                });
                if already_present {
                    outcomes[input_index] = Some(TemporalMetadataMigration::AlreadyPresent {
                        ledger_ref: stored.provenance.clone(),
                    });
                    continue;
                }

                let stored = rewrite.constellation_mut();
                for key in TEMPORAL_KEYS {
                    match request.expected_temporal.get(key) {
                        Some(value) => {
                            stored.metadata.insert(key.to_owned(), value.clone());
                        }
                        None => {
                            stored.metadata.remove(key);
                        }
                    }
                }
                pending.push(PendingTemporalMetadataRewrite {
                    input_index,
                    id,
                    panel_version: request.expected_panel_version,
                    expected_temporal: request.expected_temporal.clone(),
                    rewrite,
                });
            }

            if pending.is_empty() {
                return collect_migration_outcomes(outcomes);
            }

            let payload = migration_batch_payload(&pending)?;
            let actor = ActorId::Service("calyx-aster".to_string());
            let subject = if pending.len() == 1 {
                SubjectId::Cx(pending[0].id)
            } else {
                SubjectId::Query(b"calyx.temporal-metadata-backfill.batch.v1".to_vec())
            };
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
                for pending in &mut pending {
                    pending.rewrite.constellation_mut().provenance = ledger_ref.clone();
                    pending.rewrite.constellation().validate_schema()?;
                    rows.push(base_row(pending.id, &pending.rewrite)?);
                }
                self.commit_rows_locked(&rows)?;
                for pending in pending {
                    outcomes[pending.input_index] = Some(TemporalMetadataMigration::Backfilled {
                        ledger_ref: ledger_ref.clone(),
                    });
                }
                return collect_migration_outcomes(outcomes);
            };

            let (staged, ledger_ref) = staged.expect("hook branch returns staged rows");
            for pending in &mut pending {
                pending.rewrite.constellation_mut().provenance = ledger_ref.clone();
                pending.rewrite.constellation().validate_schema()?;
                rows.push(base_row(pending.id, &pending.rewrite)?);
            }
            self.commit_rows_locked(&rows)?;
            if let Some(hook) = hook_guard.take() {
                self.commit_persistent_ledger_staged_locked(
                    hook,
                    &staged,
                    "temporal_metadata_backfill_batch",
                )?;
            }
            for pending in pending {
                outcomes[pending.input_index] = Some(TemporalMetadataMigration::Backfilled {
                    ledger_ref: ledger_ref.clone(),
                });
            }
            collect_migration_outcomes(outcomes)
        })
    }
}

struct PendingTemporalMetadataRewrite {
    input_index: usize,
    id: CxId,
    panel_version: u32,
    expected_temporal: BTreeMap<String, String>,
    rewrite: BaseRowRewrite,
}

fn collect_migration_outcomes(
    outcomes: Vec<Option<TemporalMetadataMigration>>,
) -> Result<Vec<TemporalMetadataMigration>> {
    outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            outcome.ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "temporal metadata batch did not produce outcome for input index {index}"
                ))
            })
        })
        .collect()
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

fn base_row(id: CxId, rewrite: &BaseRowRewrite) -> Result<encode::WriteRow> {
    Ok(encode::WriteRow {
        cf: ColumnFamily::Base,
        key: base_key(id),
        value: rewrite.encode()?,
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

fn migration_batch_payload(pending: &[PendingTemporalMetadataRewrite]) -> Result<Vec<u8>> {
    if pending.len() == 1 {
        return migration_payload(
            pending[0].id,
            pending[0].panel_version,
            &pending[0].expected_temporal,
        );
    }
    let members = pending.iter().map(|item| item.id).collect::<Vec<_>>();
    let migrations = pending
        .iter()
        .map(|item| {
            serde_json::json!({
                "cx_id": item.id.to_string(),
                "panel_version": item.panel_version,
                "temporal_metadata": item.expected_temporal,
            })
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&serde_json::json!({
        "mode": "temporal-metadata-backfill-batch",
        "count": pending.len(),
        "migrations": migrations,
    }))
    .map_err(|error| {
        CalyxError::ledger_group_commit_failed(format!(
            "encode temporal metadata backfill batch payload: {error}"
        ))
    })?;
    let payload = declare_batch_members(&payload, &members)?;
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
