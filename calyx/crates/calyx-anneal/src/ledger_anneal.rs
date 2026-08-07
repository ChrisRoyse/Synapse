use std::collections::BTreeMap;
use std::sync::Mutex;

use calyx_aster::cf::{ColumnFamily, KeyRange, ledger_key, ledger_range};
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, LedgerRef, Result};
use calyx_ledger::{
    ActorId, EntryKind, LedgerAppender, LedgerCfStore, LedgerEntry, LedgerHeadAnchor, LedgerRow,
    SubjectId, decode,
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
        self.appender
            .scan_recent_entries_by_kind(EntryKind::Anneal, n)?
            .into_iter()
            .map(decode_readback)
            .collect()
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
    index: Option<&'a Mutex<AsterAnnealLedgerIndex>>,
}

/// Process-local delta index for exact kind-selective Anneal ledger reads.
///
/// The first read establishes a checked baseline from every physical Ledger
/// row. Later reads page only the ordered suffix after `indexed_through`, so an
/// absent Anneal kind remains an exact result without rescanning the mixed
/// ledger on every status request.
#[derive(Debug, Default)]
pub struct AsterAnnealLedgerIndex {
    initialized: bool,
    indexed_through: Option<u64>,
    anneal_sequences: Vec<u64>,
}

const ANNEAL_LEDGER_INDEX_PAGE_ROWS: usize = 4_096;

impl<'a, C> AsterAnnealLedgerStore<'a, C>
where
    C: Clock,
{
    pub const fn new(vault: &'a AsterVault<C>) -> Self {
        Self { vault, index: None }
    }

    pub const fn with_index(
        vault: &'a AsterVault<C>,
        index: &'a Mutex<AsterAnnealLedgerIndex>,
    ) -> Self {
        Self {
            vault,
            index: Some(index),
        }
    }
}

impl AsterAnnealLedgerIndex {
    fn refresh<C>(&mut self, vault: &AsterVault<C>) -> Result<u64>
    where
        C: Clock,
    {
        let snapshot = vault.latest_seq();
        if !self.initialized {
            let (anneal_sequences, indexed_through) =
                scan_anneal_sequence_range(vault, snapshot, &KeyRange::all(), 0)?;
            self.anneal_sequences = anneal_sequences;
            self.indexed_through = indexed_through;
            self.initialized = true;
            return Ok(snapshot);
        }

        let greatest = vault.predecessor_cf_at(
            snapshot,
            ColumnFamily::Ledger,
            &ledger_key(0),
            &ledger_key(u64::MAX),
        )?;
        let Some((greatest_key, _)) = greatest else {
            if self.indexed_through.is_some() {
                return Err(CalyxError::ledger_chain_broken(
                    "Anneal ledger delta index previously observed rows but the Ledger is now empty",
                ));
            }
            return Ok(snapshot);
        };
        let greatest_seq = parse_aster_ledger_seq(&greatest_key)?;
        if self
            .indexed_through
            .is_some_and(|indexed| greatest_seq < indexed)
        {
            return Err(CalyxError::ledger_chain_broken(format!(
                "Anneal ledger delta index regressed: indexed_through={} physical_greatest={greatest_seq}",
                self.indexed_through.expect("checked as some")
            )));
        }
        let first = match self.indexed_through {
            Some(indexed) => match indexed.checked_add(1) {
                Some(first) => first,
                None if greatest_seq == u64::MAX => return Ok(snapshot),
                None => {
                    return Err(CalyxError::ledger_chain_broken(format!(
                        "Anneal ledger delta index cannot advance after sequence {indexed}, but the physical greatest sequence is {greatest_seq}"
                    )));
                }
            },
            None => 0,
        };
        if first <= greatest_seq {
            let range = greatest_seq.checked_add(1).map_or_else(
                || KeyRange {
                    start: ledger_key(first),
                    end: None,
                },
                |end| ledger_range(first, end),
            );
            let (new_anneal, indexed_through) =
                scan_anneal_sequence_range(vault, snapshot, &range, first)?;
            if indexed_through != Some(greatest_seq) {
                return Err(CalyxError::ledger_chain_broken(format!(
                    "Anneal ledger delta index stopped before the physical head: first={first} expected_greatest={greatest_seq} indexed_through={indexed_through:?}"
                )));
            }
            self.anneal_sequences.extend(new_anneal);
            self.indexed_through = indexed_through;
        }
        Ok(snapshot)
    }

    fn recent_sequences(&self, n: usize) -> &[u64] {
        if n == usize::MAX || n >= self.anneal_sequences.len() {
            return &self.anneal_sequences;
        }
        &self.anneal_sequences[self.anneal_sequences.len() - n..]
    }
}

fn scan_anneal_sequence_range<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    range: &KeyRange,
    first_expected: u64,
) -> Result<(Vec<u64>, Option<u64>)>
where
    C: Clock,
{
    let mut anneal_sequences = Vec::new();
    let mut after_key = None;
    let mut last_seen = None;
    loop {
        let page = vault.scan_cf_range_page_at(
            snapshot,
            ColumnFamily::Ledger,
            range,
            after_key.as_deref(),
            ANNEAL_LEDGER_INDEX_PAGE_ROWS,
        )?;
        if page.is_empty() {
            break;
        }
        for (key, bytes) in &page {
            let seq = parse_aster_ledger_seq(key)?;
            let expected =
                last_seen.map_or(first_expected, |previous: u64| previous.saturating_add(1));
            if seq != expected {
                return Err(CalyxError::ledger_chain_broken(format!(
                    "Anneal ledger delta index found a sequence gap: expected={expected} found={seq}"
                )));
            }
            let entry = decode(bytes)?;
            if entry.seq != seq {
                return Err(CalyxError::ledger_chain_broken(format!(
                    "Anneal ledger delta index physical key {seq} does not match encoded seq {}",
                    entry.seq
                )));
            }
            if entry.kind == EntryKind::Anneal {
                anneal_sequences.push(seq);
            }
            last_seen = Some(seq);
        }
        after_key = page.last().map(|(key, _)| key.clone());
        if page.len() < ANNEAL_LEDGER_INDEX_PAGE_ROWS {
            break;
        }
    }
    Ok((anneal_sequences, last_seen))
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
        if n == 0 {
            return Ok(Vec::new());
        }
        if n == usize::MAX {
            return self.scan();
        }
        let snapshot = self.vault.latest_seq();
        let Some((max_key, max_bytes)) = self.vault.predecessor_cf_at(
            snapshot,
            ColumnFamily::Ledger,
            &ledger_key(0),
            &ledger_key(u64::MAX),
        )?
        else {
            return Ok(Vec::new());
        };
        let max_seq = parse_aster_ledger_seq(&max_key)?;
        let available = usize::try_from(max_seq.saturating_add(1)).unwrap_or(usize::MAX);
        let mut rows = Vec::with_capacity(n.min(available));
        rows.push(LedgerRow {
            seq: max_seq,
            bytes: max_bytes,
        });
        let mut seq = max_seq;
        while rows.len() < n && seq != 0 {
            seq -= 1;
            let key = ledger_key(seq);
            if let Some(bytes) = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Ledger, &key)?
            {
                rows.push(LedgerRow { seq, bytes });
            }
        }
        rows.reverse();
        Ok(rows)
    }

    fn scan_recent_by_kind(&self, kind: EntryKind, n: usize) -> Result<Vec<LedgerRow>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let Some(index) = self.index.filter(|_| kind == EntryKind::Anneal) else {
            let mut rows = self
                .scan()?
                .into_iter()
                .filter_map(|row| match decode(&row.bytes) {
                    Ok(entry) if entry.kind == kind => Some(Ok(row)),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<Vec<_>>>()?;
            if n < rows.len() {
                rows.drain(0..rows.len() - n);
            }
            return Ok(rows);
        };
        let (snapshot, sequences) = {
            let mut index = index.lock().map_err(|_| {
                CalyxError::ledger_corrupt(
                    "Anneal ledger delta index lock is poisoned; restart the process and inspect the preceding panic",
                )
            })?;
            let snapshot = index.refresh(self.vault)?;
            (snapshot, index.recent_sequences(n).to_vec())
        };
        sequences
            .into_iter()
            .map(|seq| {
                let bytes = self
                    .vault
                    .read_cf_at(snapshot, ColumnFamily::Ledger, &ledger_key(seq))?
                    .ok_or_else(|| {
                        CalyxError::ledger_chain_broken(format!(
                            "Anneal ledger delta index points at missing physical seq {seq}"
                        ))
                    })?;
                let entry = decode(&bytes)?;
                if entry.seq != seq || entry.kind != EntryKind::Anneal {
                    return Err(CalyxError::ledger_corrupt(format!(
                        "Anneal ledger delta index points at seq {seq} encoded as seq {} kind {:?}",
                        entry.seq, entry.kind
                    )));
                }
                Ok(LedgerRow { seq, bytes })
            })
            .collect()
    }

    fn read_seq(&self, seq: u64) -> Result<Option<LedgerRow>> {
        self.vault
            .read_cf_at(
                self.vault.latest_seq(),
                ColumnFamily::Ledger,
                &ledger_key(seq),
            )
            .map(|bytes| bytes.map(|bytes| LedgerRow { seq, bytes }))
    }

    fn head_anchor(&self) -> Result<Option<LedgerHeadAnchor>> {
        self.vault.verified_ledger_head_anchor()
    }

    fn put_head_anchor(&mut self, anchor: &LedgerHeadAnchor) -> Result<()> {
        self.vault.validate_committed_ledger_head_anchor(anchor)
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
