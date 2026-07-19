use std::cmp::Reverse;
use std::collections::BTreeSet;

use calyx_core::{CalyxError, Clock, Constellation, CxId, Result, SlotId, VaultId, VaultStore};
use calyx_ledger::{MemoryLedgerStore, VerifyResult, decode as decode_ledger, verify_chain};
use serde::{Deserialize, Serialize};

use super::{DedupAction, EpochSecs, OccurrenceId, dedup_error};
use crate::cf::{ColumnFamily, recurrence_key};
use crate::recurrence::{
    FREQUENCY_SCALAR, Occurrence, StoredRecurrenceRow, encode_recurrence_row, read_series,
    recurrence_summary_key,
};
use crate::vault::AsterVault;

pub const CALYX_DEDUP_WRONG_VAULT: &str = "CALYX_DEDUP_WRONG_VAULT";
pub const CALYX_DEDUP_UNDO_MISSING_RESTORE: &str = "CALYX_DEDUP_UNDO_MISSING_RESTORE";
pub const CALYX_DEDUP_UNDO_EMPTY_TOKEN: &str = "CALYX_DEDUP_UNDO_EMPTY_TOKEN";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeRecord {
    pub seq: u64,
    pub at: EpochSecs,
    pub merged_from: CxId,
    pub per_slot_cos: Vec<(SlotId, f32)>,
    pub recurrence_signature: bool,
    pub anchor_conflict: bool,
    pub action: DedupAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupUndoRecord {
    pub seq: u64,
    pub reversal: ReversalToken,
    pub restored: Vec<CxId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DedupAuditReport {
    pub cx_id: CxId,
    pub merges: Vec<MergeRecord>,
    pub undo_entries: Vec<DedupUndoRecord>,
    pub occurrences: Vec<Occurrence>,
    pub reversal_token: ReversalToken,
    pub anchor_conflict_blocks: Vec<CxId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReversalToken {
    pub vault_id: VaultId,
    pub target_cx_id: CxId,
    pub ledger_seq_start: u64,
    pub ledger_seq_end: u64,
    pub snapshot_cx_ids: Vec<CxId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DedupRestoreSnapshot {
    pub version: u8,
    pub vault_id: VaultId,
    pub merged_into: CxId,
    pub merged_from: CxId,
    pub candidate: Constellation,
    pub before_base: Option<Constellation>,
    pub recurrence_tombstones: Vec<OccurrenceId>,
}

impl DedupRestoreSnapshot {
    pub(crate) fn new(
        vault_id: VaultId,
        merged_into: CxId,
        candidate: Constellation,
        before_base: Option<Constellation>,
        recurrence_tombstones: Vec<OccurrenceId>,
    ) -> Self {
        Self {
            version: 1,
            vault_id,
            merged_into,
            merged_from: candidate.cx_id,
            candidate,
            before_base,
            recurrence_tombstones,
        }
    }
}

pub fn dedup_audit<C>(vault: &AsterVault<C>, cx_id: CxId) -> Result<DedupAuditReport>
where
    C: Clock,
{
    let entries = dedup_ledger_entries(vault)?;
    let mut merges = Vec::new();
    for entry in &entries {
        if entry.payload.dedup_result.as_deref() != Some("DedupMerge") {
            continue;
        }
        if entry.payload.dedup_into_id != Some(cx_id) {
            continue;
        }
        merges.push(merge_record(entry)?);
    }
    let undo_entries = undo_records(&entries, cx_id)?;
    let occurrences = read_series(vault, cx_id)?.occurrences;
    let anchor_conflict_blocks = anchor_conflict_blocks(vault, cx_id)?;
    let reversal_token = reversal_token(vault.vault_id(), cx_id, &merges);
    Ok(DedupAuditReport {
        cx_id,
        merges,
        undo_entries,
        occurrences,
        reversal_token,
        anchor_conflict_blocks,
    })
}

pub fn dedup_undo<C>(vault: &AsterVault<C>, token: &ReversalToken) -> Result<Vec<CxId>>
where
    C: Clock,
{
    vault.with_recurrence_write_lock(|| dedup_undo_locked(vault, token))
}

fn dedup_undo_locked<C>(vault: &AsterVault<C>, token: &ReversalToken) -> Result<Vec<CxId>>
where
    C: Clock,
{
    validate_token_vault(vault, token)?;
    if let Some(restored) = already_undone(vault, token)? {
        return Ok(restored);
    }
    let entries = dedup_ledger_entries(vault)?;
    let mut restore_entries = entries
        .into_iter()
        .filter(|entry| token_contains(token, entry.seq))
        .filter(|entry| entry.payload.dedup_result.as_deref() == Some("DedupMerge"))
        .filter(|entry| entry.payload.dedup_into_id == Some(token.target_cx_id))
        .collect::<Vec<_>>();
    restore_entries.sort_by_key(|entry| Reverse(entry.seq));
    if restore_entries.is_empty() && token_has_merges(token) {
        return Err(dedup_error(
            CALYX_DEDUP_UNDO_MISSING_RESTORE,
            "reversal token has no matching merge ledger entries for its target",
        ));
    }
    let mut restored = BTreeSet::new();
    let mut restored_cx = Vec::new();
    let mut updated_bases = Vec::new();
    let mut recurrence_rows = Vec::new();
    let mut recurrence_resets = BTreeSet::new();
    for entry in restore_entries {
        let snapshot = entry.payload.restore.ok_or_else(|| {
            dedup_error(
                CALYX_DEDUP_UNDO_MISSING_RESTORE,
                format!("ledger seq {} has no restore snapshot", entry.seq),
            )
        })?;
        validate_snapshot_vault(vault, token, &snapshot)?;
        if snapshot.merged_into != token.target_cx_id {
            return Err(CalyxError::ledger_chain_broken(format!(
                "restore snapshot target {} != token target {}",
                snapshot.merged_into, token.target_cx_id
            )));
        }
        if snapshot.candidate.cx_id != snapshot.merged_into {
            restored.insert(snapshot.candidate.cx_id);
            restored_cx.push(snapshot.candidate);
        } else {
            restored.insert(snapshot.merged_from);
        }
        let resets_recurrence = !snapshot.recurrence_tombstones.is_empty();
        if let Some(mut before_base) = snapshot.before_base {
            if resets_recurrence {
                before_base.scalars.remove(FREQUENCY_SCALAR);
            }
            updated_bases.push(before_base);
        }
        if resets_recurrence {
            recurrence_resets.insert(snapshot.merged_into);
        }
        for id in snapshot.recurrence_tombstones {
            recurrence_rows.push((
                recurrence_key(snapshot.merged_into, id.0),
                encode_recurrence_row(&StoredRecurrenceRow::Tombstone { id })?,
            ));
        }
    }
    for cx_id in recurrence_resets {
        let id = OccurrenceId(0);
        recurrence_rows.push((
            recurrence_key(cx_id, id.0),
            encode_recurrence_row(&StoredRecurrenceRow::Tombstone { id })?,
        ));
        let id = OccurrenceId(u64::MAX);
        recurrence_rows.push((
            recurrence_summary_key(cx_id),
            encode_recurrence_row(&StoredRecurrenceRow::Tombstone { id })?,
        ));
    }
    let restored = restored.into_iter().collect::<Vec<_>>();
    let payload = undo_payload(token, &restored)?;
    let subject = token_subject(token)?;
    vault.commit_dedup_undo_locked(
        restored_cx,
        updated_bases,
        recurrence_rows,
        subject,
        payload,
    )?;
    Ok(restored)
}

#[derive(Clone, Debug)]
struct DedupLedgerEntry {
    seq: u64,
    payload: DedupLedgerPayload,
}

#[derive(Clone, Debug, Deserialize)]
struct DedupLedgerPayload {
    cx_id: Option<CxId>,
    event_time_secs: Option<i64>,
    dedup_result: Option<String>,
    dedup_decision: Option<String>,
    dedup_action: Option<DedupAction>,
    dedup_into_id: Option<CxId>,
    recurrence_signature: Option<bool>,
    per_slot_cos: Option<Vec<PerSlotCosPayload>>,
    restore: Option<DedupRestoreSnapshot>,
    reversal: Option<ReversalToken>,
    restored: Option<Vec<CxId>>,
}

#[derive(Clone, Debug, Deserialize)]
struct PerSlotCosPayload {
    slot: SlotId,
    cos: f32,
}

fn dedup_ledger_entries<C>(vault: &AsterVault<C>) -> Result<Vec<DedupLedgerEntry>>
where
    C: Clock,
{
    let mut entries = Vec::new();
    let rows = ledger_rows(vault)?;
    ensure_ledger_chain(&rows)?;
    for (seq, bytes) in rows {
        let entry = decode_ledger(&bytes)?;
        let Ok(payload) = serde_json::from_slice::<DedupLedgerPayload>(&entry.payload) else {
            continue;
        };
        if payload.dedup_result.is_some() {
            entries.push(DedupLedgerEntry { seq, payload });
        }
    }
    entries.sort_by_key(|entry| entry.seq);
    Ok(entries)
}

fn ledger_rows<C>(vault: &AsterVault<C>) -> Result<Vec<(u64, Vec<u8>)>>
where
    C: Clock,
{
    vault
        .scan_cf_at(vault.snapshot(), ColumnFamily::Ledger)?
        .into_iter()
        .map(|(key, bytes)| ledger_seq_from_key(&key).map(|seq| (seq, bytes)))
        .collect()
}

fn ledger_seq_from_key(key: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| CalyxError::ledger_corrupt(format!("ledger key length {} != 8", key.len())))?;
    Ok(u64::from_be_bytes(bytes))
}

fn ensure_ledger_chain(rows: &[(u64, Vec<u8>)]) -> Result<()> {
    let Some(max_seq) = rows.iter().map(|(seq, _)| *seq).max() else {
        return Ok(());
    };
    let end = max_seq
        .checked_add(1)
        .ok_or_else(|| CalyxError::ledger_chain_broken("ledger sequence exhausted"))?;
    let mut store = MemoryLedgerStore::default();
    for (seq, bytes) in rows {
        store.insert_raw(*seq, bytes.clone());
    }
    match verify_chain(&store, 0..end)? {
        VerifyResult::Intact { count } if count == rows.len() as u64 => Ok(()),
        VerifyResult::Intact { count } => Err(CalyxError::ledger_chain_broken(format!(
            "ledger row count {} != verified count {count}",
            rows.len()
        ))),
        VerifyResult::Broken { at_seq, .. } => Err(CalyxError::ledger_chain_broken(format!(
            "ledger chain broken at seq {at_seq}"
        ))),
        VerifyResult::Corrupt { at_seq, reason } => Err(CalyxError::ledger_corrupt(format!(
            "ledger row corrupt at seq {at_seq}: {reason}"
        ))),
    }
}

fn merge_record(entry: &DedupLedgerEntry) -> Result<MergeRecord> {
    let payload = &entry.payload;
    let merged_from = payload
        .restore
        .as_ref()
        .map(|restore| restore.merged_from)
        .or(payload.cx_id)
        .ok_or_else(|| CalyxError::ledger_corrupt("dedup merge payload missing cx_id"))?;
    Ok(MergeRecord {
        seq: entry.seq,
        at: EpochSecs(payload.event_time_secs.unwrap_or_default()),
        merged_from,
        per_slot_cos: payload
            .per_slot_cos
            .as_ref()
            .map(|values| values.iter().map(|value| (value.slot, value.cos)).collect())
            .unwrap_or_default(),
        recurrence_signature: payload.recurrence_signature.unwrap_or(false),
        anchor_conflict: payload.dedup_decision.as_deref() == Some("AnchorConflict"),
        action: payload
            .dedup_action
            .clone()
            .ok_or_else(|| CalyxError::ledger_corrupt("dedup merge payload missing action"))?,
    })
}

fn undo_records(entries: &[DedupLedgerEntry], cx_id: CxId) -> Result<Vec<DedupUndoRecord>> {
    let mut records = Vec::new();
    for entry in entries {
        if entry.payload.dedup_result.as_deref() != Some("DedupUndo") {
            continue;
        }
        let Some(reversal) = entry.payload.reversal.as_ref() else {
            continue;
        };
        if reversal.target_cx_id != cx_id {
            continue;
        }
        records.push(DedupUndoRecord {
            seq: entry.seq,
            reversal: reversal.clone(),
            restored: undo_restored(entry)?,
        });
    }
    Ok(records)
}

/// `undo_payload` always writes the `restored` list, so a `DedupUndo` entry
/// without one can only come from corruption or a foreign writer.
fn undo_restored(entry: &DedupLedgerEntry) -> Result<Vec<CxId>> {
    entry.payload.restored.clone().ok_or_else(|| {
        CalyxError::ledger_corrupt(format!(
            "DedupUndo ledger entry seq {} missing restored CxIds",
            entry.seq
        ))
    })
}

fn reversal_token(vault_id: VaultId, cx_id: CxId, merges: &[MergeRecord]) -> ReversalToken {
    let mut snapshot_cx_ids = BTreeSet::from([cx_id]);
    for merge in merges {
        snapshot_cx_ids.insert(merge.merged_from);
    }
    ReversalToken {
        vault_id,
        target_cx_id: cx_id,
        ledger_seq_start: merges.iter().map(|merge| merge.seq).min().unwrap_or(0),
        ledger_seq_end: merges.iter().map(|merge| merge.seq).max().unwrap_or(0),
        snapshot_cx_ids: snapshot_cx_ids.into_iter().collect(),
    }
}

fn anchor_conflict_blocks<C>(vault: &AsterVault<C>, cx_id: CxId) -> Result<Vec<CxId>>
where
    C: Clock,
{
    let mut blocks = BTreeSet::new();
    let key = super::contested_with_key(cx_id);
    if let Some(bytes) = vault.read_cf_at(vault.snapshot(), ColumnFamily::Online, &key)? {
        blocks.insert(super::decode_contested_with(&bytes)?.contested_with);
    }
    Ok(blocks.into_iter().collect())
}

fn already_undone<C>(vault: &AsterVault<C>, token: &ReversalToken) -> Result<Option<Vec<CxId>>>
where
    C: Clock,
{
    for entry in dedup_ledger_entries(vault)? {
        if entry.payload.dedup_result.as_deref() == Some("DedupUndo")
            && entry.payload.reversal.as_ref() == Some(token)
        {
            return Ok(Some(undo_restored(&entry)?));
        }
    }
    Ok(None)
}

fn validate_token_vault<C>(vault: &AsterVault<C>, token: &ReversalToken) -> Result<()>
where
    C: Clock,
{
    if token.snapshot_cx_ids.is_empty() {
        return Err(dedup_error(
            CALYX_DEDUP_UNDO_EMPTY_TOKEN,
            "reversal token has no snapshot CxIds",
        ));
    }
    if !token.snapshot_cx_ids.contains(&token.target_cx_id) {
        return Err(CalyxError::ledger_corrupt(
            "reversal token snapshot set does not contain target CxId",
        ));
    }
    if token.vault_id != vault.vault_id() {
        return Err(dedup_error(
            CALYX_DEDUP_WRONG_VAULT,
            format!(
                "token vault {} != active vault {}",
                token.vault_id,
                vault.vault_id()
            ),
        ));
    }
    Ok(())
}

fn validate_snapshot_vault<C>(
    vault: &AsterVault<C>,
    token: &ReversalToken,
    snapshot: &DedupRestoreSnapshot,
) -> Result<()>
where
    C: Clock,
{
    if snapshot.vault_id == token.vault_id && snapshot.vault_id == vault.vault_id() {
        Ok(())
    } else {
        Err(dedup_error(
            CALYX_DEDUP_WRONG_VAULT,
            "restore snapshot vault does not match reversal token",
        ))
    }
}

fn token_contains(token: &ReversalToken, seq: u64) -> bool {
    token.ledger_seq_start <= seq && seq <= token.ledger_seq_end
}

fn token_has_merges(token: &ReversalToken) -> bool {
    token.snapshot_cx_ids.len() > 1 || token.ledger_seq_start != 0 || token.ledger_seq_end != 0
}

fn token_subject(token: &ReversalToken) -> Result<CxId> {
    if token.snapshot_cx_ids.contains(&token.target_cx_id) {
        return Ok(token.target_cx_id);
    }
    Err(dedup_error(
        CALYX_DEDUP_UNDO_EMPTY_TOKEN,
        "reversal token target is missing from snapshot CxIds",
    ))
}

fn undo_payload(token: &ReversalToken, restored: &[CxId]) -> Result<Vec<u8>> {
    let value = serde_json::json!({
        "dedup_result": "DedupUndo",
        "reversal": token,
        "restored": restored,
    });
    serde_json::to_vec(&value)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("encode undo payload: {error}")))
}
