use std::collections::BTreeMap;

use calyx_aster::cf::{ColumnFamily, KeyRange, ledger_key};
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, LedgerRef, Result};
use calyx_ledger::{
    ActorId, EntryKind, LedgerAppender, LedgerCfStore, LedgerEntry, LedgerRow, SubjectId,
};
use serde::{Deserialize, Serialize};

use crate::propose::AdmissionRecord;
use crate::{ChangeId, LogicalTime, MetricSnapshot};

pub const ANNEAL_LEDGER_PAYLOAD_TAG: &str = "anneal_event_v1";
pub const MAX_ANNEAL_LEDGER_PAYLOAD_BYTES: usize = 16 * 1024;
pub const CALYX_LEDGER_ENTRY_TOO_LARGE: &str = "CALYX_LEDGER_ENTRY_TOO_LARGE";
pub const CALYX_ANNEAL_LEDGER_INVALID_ENTRY: &str = "CALYX_ANNEAL_LEDGER_INVALID_ENTRY";
pub const CALYX_ASTER_CF_UNAVAILABLE: &str = "CALYX_ASTER_CF_UNAVAILABLE";

/// Ledger event type for Anneal audit entries.
///
/// This name avoids the existing `AnnealAction` shadow-execution trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnealLedgerAction {
    Promote,
    Revert,
    Propose,
    #[serde(rename = "LensAdmitted")]
    LensAdmitted,
    #[serde(rename = "LensRejected")]
    LensRejected,
    Park,
    DegradeChange,
    FaultEvent,
    Rebuild,
    BaseCorruptAlert,
    BaseRestored,
    Recalibrate,
    TauRecalibrated,
    TauRecalibrationReverted,
    LensPark,
    LensUnpark,
    MistakeUpdate,
    HeadUpdate,
    HeadUpdateReverted,
    OperatorPromoted,
    OperatorReverted,
    SleepPassDeferred,
    OutcomeReward,
    OutcomeContradiction,
    #[serde(rename = "autotune_ab")]
    AutotuneAB,
    #[serde(rename = "autotune_abandoned")]
    AutotuneAbandoned,
    #[serde(rename = "autotune_promote")]
    AutotunePromote,
    #[serde(rename = "GoodhartPassed")]
    GoodhartPassed,
    #[serde(rename = "GoodhartFailed")]
    GoodhartFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnealFaultLedgerDetails {
    pub fault_kind: String,
    pub recommendation: String,
    pub component_kind: String,
    pub component_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_id: Option<String>,
}

impl AnnealFaultLedgerDetails {
    pub fn component_label(&self) -> String {
        match self.component_kind.as_str() {
            "ann_index" => format!(
                "AnnIndex(panel_{}/slot_{})",
                self.panel_version.unwrap_or_default(),
                self.slot_id.unwrap_or_default()
            ),
            "guard_profile" => format!(
                "GuardProfile(panel_{}/slot_{})",
                self.panel_version.unwrap_or_default(),
                self.slot_id.unwrap_or_default()
            ),
            "lens_endpoint" => self
                .lens_id
                .as_ref()
                .map(|lens_id| format!("LensEndpoint({lens_id})"))
                .unwrap_or_else(|| format!("LensEndpoint(hash={})", self.component_hash)),
            "kernel_index" => self
                .scope_hash
                .as_ref()
                .map(|hash| format!("KernelIndex(scope_hash={hash})"))
                .unwrap_or_else(|| format!("KernelIndex(hash={})", self.component_hash)),
            "base_shard" => self
                .shard_id
                .as_ref()
                .map(|shard_id| format!("BaseShard({shard_id})"))
                .unwrap_or_else(|| format!("BaseShard(hash={})", self.component_hash)),
            _ => format!("{}({})", self.component_kind, self.component_hash),
        }
    }
}

/// Hash-only Anneal audit payload that is safe for the ledger redaction policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnnealLedgerEntry {
    pub action: AnnealLedgerAction,
    pub change_id: ChangeId,
    pub artifact_id: String,
    pub prior_ptr_hash: [u8; 32],
    pub candidate_ptr_hash: [u8; 32],
    pub metrics: MetricSnapshot,
    pub ts: LogicalTime,
    pub description: String,
    pub fault: Option<AnnealFaultLedgerDetails>,
    pub proposal: Option<AdmissionRecord>,
    pub details: Option<serde_json::Value>,
    pub prev_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnnealLedgerReadback {
    pub ledger_ref: LedgerRef,
    pub entry: AnnealLedgerEntry,
}

pub struct AnnealLedger<S, C>
where
    S: LedgerCfStore,
    C: Clock,
{
    appender: LedgerAppender<S, C>,
    actor: ActorId,
}

impl<S, C> AnnealLedger<S, C>
where
    S: LedgerCfStore,
    C: Clock,
{
    pub fn new(appender: LedgerAppender<S, C>, actor: ActorId) -> Result<Self> {
        actor.validate()?;
        Ok(Self { appender, actor })
    }

    pub fn write(&mut self, mut entry: AnnealLedgerEntry) -> Result<LedgerRef> {
        self.appender.refresh_tip_from_store()?;
        let chain_prev = self.appender.prev_hash();
        if let Some(expected) = entry.prev_hash
            && expected != chain_prev
        {
            return Err(CalyxError::ledger_chain_broken(
                "Anneal entry prev_hash does not match ledger tip",
            ));
        }
        entry.prev_hash = Some(chain_prev);
        let payload = encode_payload(&entry)?;
        self.appender.append(
            EntryKind::Anneal,
            anneal_subject(entry.change_id),
            payload,
            self.actor.clone(),
        )
    }

    pub fn read_recent(&self, n: usize) -> Result<Vec<AnnealLedgerEntry>> {
        Ok(self
            .read_recent_with_refs(n)?
            .into_iter()
            .map(|readback| readback.entry)
            .collect())
    }

    pub fn read_recent_with_refs(&self, n: usize) -> Result<Vec<AnnealLedgerReadback>> {
        if n == usize::MAX {
            return self.scan_anneal_entries();
        }
        let mut limit = n.max(1);
        loop {
            let ledger_entries = self.appender.scan_recent_entries(limit)?;
            let saw_all_rows = ledger_entries.len() < limit;
            let mut anneal_entries = ledger_entries
                .into_iter()
                .filter(|entry| entry.kind == EntryKind::Anneal)
                .map(decode_readback)
                .collect::<Result<Vec<_>>>()?;
            if anneal_entries.len() >= n || saw_all_rows {
                if n < anneal_entries.len() {
                    anneal_entries.drain(0..anneal_entries.len() - n);
                }
                return Ok(anneal_entries);
            }
            limit = limit.saturating_mul(2);
            if limit == usize::MAX {
                return self.scan_anneal_entries();
            }
        }
    }

    pub fn find_by_change_id(&self, id: ChangeId) -> Result<Option<AnnealLedgerEntry>> {
        Ok(self
            .find_by_change_id_with_ref(id)?
            .map(|readback| readback.entry))
    }

    pub fn find_by_change_id_with_ref(&self, id: ChangeId) -> Result<Option<AnnealLedgerReadback>> {
        Ok(self
            .scan_anneal_entries()?
            .into_iter()
            .rev()
            .find(|readback| readback.entry.change_id == id))
    }

    pub fn appender(&self) -> &LedgerAppender<S, C> {
        &self.appender
    }

    pub fn appender_mut(&mut self) -> &mut LedgerAppender<S, C> {
        &mut self.appender
    }

    fn scan_anneal_entries(&self) -> Result<Vec<AnnealLedgerReadback>> {
        self.appender
            .scan_entries()?
            .into_iter()
            .filter(|entry| entry.kind == EntryKind::Anneal)
            .map(decode_readback)
            .collect()
    }
}

/// Writable adapter from an Aster vault's `ledger` CF to `LedgerAppender`.
pub struct AsterAnnealLedgerStore<'a, C>
where
    C: Clock,
{
    vault: &'a AsterVault<C>,
}

impl<'a, C> AsterAnnealLedgerStore<'a, C>
where
    C: Clock,
{
    pub const fn new(vault: &'a AsterVault<C>) -> Self {
        Self { vault }
    }
}

impl<C> LedgerCfStore for AsterAnnealLedgerStore<'_, C>
where
    C: Clock,
{
    fn scan(&self) -> Result<Vec<LedgerRow>> {
        let mut rows = BTreeMap::new();
        for (key, bytes) in self
            .vault
            .scan_cf_at(self.vault.latest_seq(), ColumnFamily::Ledger)?
        {
            let seq = parse_aster_ledger_seq(&key)?;
            if rows.insert(seq, bytes).is_some() {
                return Err(CalyxError::ledger_corrupt(format!(
                    "duplicate Aster ledger row for seq {seq}"
                )));
            }
        }
        Ok(rows
            .into_iter()
            .map(|(seq, bytes)| LedgerRow { seq, bytes })
            .collect())
    }

    fn scan_recent(&self, n: usize) -> Result<Vec<LedgerRow>> {
        if n == usize::MAX {
            return self.scan();
        }
        let snapshot = self.vault.latest_seq();
        let keys =
            self.vault
                .scan_cf_range_keys_at(snapshot, ColumnFamily::Ledger, &KeyRange::all())?;
        let Some(max_seq) = keys
            .iter()
            .map(|key| parse_aster_ledger_seq(key))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
        else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::with_capacity(n.min(max_seq as usize + 1));
        let mut seq = max_seq;
        loop {
            let key = ledger_key(seq);
            if let Some(bytes) = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Ledger, &key)?
            {
                rows.push(LedgerRow { seq, bytes });
                if rows.len() == n {
                    break;
                }
            }
            if seq == 0 {
                break;
            }
            seq -= 1;
        }
        rows.reverse();
        Ok(rows)
    }

    fn put_new(&mut self, seq: u64, bytes: &[u8]) -> Result<()> {
        self.vault.append_external_ledger_row(seq, bytes)
    }
}

#[derive(Serialize, Deserialize)]
struct AnnealLedgerPayload {
    kind: String,
    tag: String,
    action: AnnealLedgerAction,
    change_id: u64,
    artifact_id: String,
    prior_ptr_hash: String,
    candidate_ptr_hash: String,
    metrics: MetricSnapshot,
    ts: LogicalTime,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fault: Option<AnnealFaultLedgerDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposal: Option<AdmissionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
    prev_hash: Option<String>,
}

fn encode_payload(entry: &AnnealLedgerEntry) -> Result<Vec<u8>> {
    let payload = AnnealLedgerPayload {
        kind: "Anneal".to_string(),
        tag: ANNEAL_LEDGER_PAYLOAD_TAG.to_string(),
        action: entry.action,
        change_id: entry.change_id.0,
        artifact_id: entry.artifact_id.clone(),
        prior_ptr_hash: hex32(&entry.prior_ptr_hash),
        candidate_ptr_hash: hex32(&entry.candidate_ptr_hash),
        metrics: entry.metrics.clone(),
        ts: entry.ts,
        description: entry.description.clone(),
        fault: entry.fault.clone(),
        proposal: entry.proposal.clone(),
        details: entry.details.clone(),
        prev_hash: entry.prev_hash.as_ref().map(hex32),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|error| invalid_entry(format!("serialize Anneal ledger payload: {error}")))?;
    if bytes.len() > MAX_ANNEAL_LEDGER_PAYLOAD_BYTES {
        return Err(entry_too_large(bytes.len()));
    }
    Ok(bytes)
}

fn decode_readback(entry: LedgerEntry) -> Result<AnnealLedgerReadback> {
    let decoded = decode_payload(&entry.payload)?;
    Ok(AnnealLedgerReadback {
        ledger_ref: LedgerRef {
            seq: entry.seq,
            hash: entry.entry_hash,
        },
        entry: decoded,
    })
}

fn decode_payload(payload: &[u8]) -> Result<AnnealLedgerEntry> {
    let payload = serde_json::from_slice::<AnnealLedgerPayload>(payload)
        .map_err(|error| invalid_entry(format!("decode Anneal ledger payload: {error}")))?;
    if payload.kind != "Anneal" || payload.tag != ANNEAL_LEDGER_PAYLOAD_TAG {
        return Err(invalid_entry(
            "Anneal ledger payload has invalid kind or tag",
        ));
    }
    Ok(AnnealLedgerEntry {
        action: payload.action,
        change_id: ChangeId(payload.change_id),
        artifact_id: payload.artifact_id,
        prior_ptr_hash: decode_hex32(&payload.prior_ptr_hash, "prior_ptr_hash")?,
        candidate_ptr_hash: decode_hex32(&payload.candidate_ptr_hash, "candidate_ptr_hash")?,
        metrics: payload.metrics,
        ts: payload.ts,
        description: payload.description,
        fault: payload.fault,
        proposal: payload.proposal,
        details: payload.details,
        prev_hash: match payload.prev_hash {
            Some(value) => Some(decode_hex32(&value, "prev_hash")?),
            None => None,
        },
    })
}

pub fn decode_anneal_ledger_payload(payload: &[u8]) -> Result<AnnealLedgerEntry> {
    decode_payload(payload)
}

fn anneal_subject(change_id: ChangeId) -> SubjectId {
    let mut subject = Vec::with_capacity(15);
    subject.extend_from_slice(b"anneal\0");
    subject.extend_from_slice(&change_id.0.to_be_bytes());
    SubjectId::Kernel(subject)
}

fn parse_aster_ledger_seq(key: &[u8]) -> Result<u64> {
    let key: [u8; 8] = key.try_into().map_err(|_| {
        CalyxError::ledger_corrupt(format!(
            "Aster ledger CF key has {} bytes, expected 8",
            key.len()
        ))
    })?;
    Ok(u64::from_be_bytes(key))
}

fn decode_hex32(value: &str, field: &str) -> Result<[u8; 32]> {
    if value.len() != 64 {
        return Err(invalid_entry(format!(
            "{field} has {} hex chars, expected 64",
            value.len()
        )));
    }
    let mut out = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        out[index] = (hex_value(chunk[0], field)? << 4) | hex_value(chunk[1], field)?;
    }
    Ok(out)
}

fn hex_value(byte: u8, field: &str) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(invalid_entry(format!("{field} contains non-hex byte"))),
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn entry_too_large(len: usize) -> CalyxError {
    CalyxError {
        code: CALYX_LEDGER_ENTRY_TOO_LARGE,
        message: format!(
            "Anneal ledger payload has {len} bytes, max {MAX_ANNEAL_LEDGER_PAYLOAD_BYTES}"
        ),
        remediation: "store hash/id-only Anneal payload fields and shorten description",
    }
}

fn invalid_entry(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_LEDGER_INVALID_ENTRY,
        message: message.into(),
        remediation: "repair or quarantine invalid Anneal ledger payload bytes",
    }
}
