//! Quarantine-aware Ledger provenance query surface.

use std::ops::Range;
use std::str::FromStr;

use calyx_core::{CalyxError, CalyxWarning, CxId, LensId, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::append::{LedgerCfStore, LedgerRow};
use crate::codec::decode;
use crate::entry::{ActorId, LedgerEntry, SubjectId};
use crate::kind::EntryKind;
use crate::verify::DecodedLedgerSnapshot;

mod links;
mod mentions;
use links::*;
pub use mentions::entry_cx_mentions;
use mentions::entry_mentions_cx;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFilter {
    pub kind: Option<EntryKind>,
    pub actor: Option<ActorId>,
    pub ts_range: Option<(u64, u64)>,
    pub seq_range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerTrace {
    pub answer_entry: Option<LedgerEntry>,
    pub kernel_entry: Option<LedgerEntry>,
    pub guard_entry: Option<LedgerEntry>,
    pub path: Vec<AnswerTraceHop>,
    /// Producer-owned fusion metadata, retained verbatim for provenance.
    /// Ledger does not pretend it can replay this payload: replay requires the
    /// exact immutable query input, panel/lenses, index generation and candidate
    /// universe, none of which a generic JSON weight object establishes.
    pub fusion_weights: Option<Value>,
    pub guard_result: Option<Value>,
    pub freshness_ts: Option<u64>,
    pub complete: bool,
    pub warnings: Vec<CalyxWarning>,
}

impl AnswerTrace {
    pub fn is_trusted(&self) -> bool {
        self.complete && self.warnings.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerTraceHop {
    pub cx_id: CxId,
    pub from_cx_id: Option<CxId>,
    pub hop: u32,
    pub score: f32,
    pub lens_id: Option<LensId>,
    pub ledger_seq: u64,
}

pub trait QuarantineLookup {
    fn contains_quarantined(&self, range: Range<u64>) -> Result<bool>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuarantineSet {
    ranges: Vec<Range<u64>>,
}

impl QuarantineSet {
    pub fn from_ranges(ranges: impl IntoIterator<Item = Range<u64>>) -> Result<Self> {
        let mut ranges = ranges.into_iter().collect::<Vec<_>>();
        for range in &ranges {
            if range.start >= range.end {
                return Err(CalyxError::ledger_chain_broken(
                    "quarantine range must be non-empty",
                ));
            }
        }
        ranges.sort_by_key(|range| (range.start, range.end));
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        Ok(Self { ranges: merged })
    }
}

impl QuarantineLookup for QuarantineSet {
    fn contains_quarantined(&self, range: Range<u64>) -> Result<bool> {
        let candidate = self
            .ranges
            .partition_point(|quarantine| quarantine.end <= range.start);
        Ok(self
            .ranges
            .get(candidate)
            .is_some_and(|quarantine| ranges_overlap(quarantine, &range)))
    }
}

pub fn get_provenance(
    cf_reader: &impl LedgerCfStore,
    quarantine: &dyn QuarantineLookup,
    cx_id: CxId,
) -> Result<Vec<LedgerEntry>> {
    let mut out = Vec::new();
    for entry in decode_entries(cf_reader, quarantine)? {
        if entry_mentions_cx(&entry, cx_id) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Returns the entries mentioning `cx_id` from an already decoded, coherent
/// ledger snapshot. This avoids reacquiring and cloning a store view when a
/// request needs more than one ledger projection.
pub fn get_provenance_from_snapshot(
    snapshot: &DecodedLedgerSnapshot,
    quarantine: &dyn QuarantineLookup,
    cx_id: CxId,
) -> Result<Vec<LedgerEntry>> {
    Ok(decode_entries_from_snapshot(snapshot, quarantine)?
        .into_iter()
        .filter(|entry| entry_mentions_cx(entry, cx_id))
        .collect())
}

pub fn get_answer_trace(
    cf_reader: &impl LedgerCfStore,
    quarantine: &dyn QuarantineLookup,
    answer_id: &[u8],
) -> Result<AnswerTrace> {
    let entries = decode_entries(cf_reader, quarantine)?;
    answer_trace_from_entries(&entries, quarantine, answer_id)
}

/// Derives an answer trace from entries already decoded for chain
/// verification, avoiding a second ledger scan and decode pass.
pub fn get_answer_trace_from_snapshot(
    snapshot: &DecodedLedgerSnapshot,
    quarantine: &dyn QuarantineLookup,
    answer_id: &[u8],
) -> Result<AnswerTrace> {
    let entries = decode_entries_from_snapshot(snapshot, quarantine)?;
    answer_trace_from_entries(&entries, quarantine, answer_id)
}

pub fn answer_trace_from_entries(
    entries: &[LedgerEntry],
    quarantine: &dyn QuarantineLookup,
    answer_id: &[u8],
) -> Result<AnswerTrace> {
    let mut answers = entries
        .iter()
        .filter(|&entry| {
            entry.kind == EntryKind::Answer && answer_subject_matches(entry, answer_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    answers.sort_by_key(|entry| entry.seq);
    for entry in &answers {
        ensure_seq_not_quarantined(quarantine, entry.seq)?;
    }
    if answers.is_empty() {
        return Ok(unprovenanced_trace("answer_trace.missing"));
    }

    let payloads = answers
        .iter()
        .map(|entry| answer_payload(entry).map(|payload| (entry, payload)))
        .collect::<Result<Vec<_>>>()?;
    let complete_payload = payloads
        .iter()
        .rev()
        .find(|(_, payload)| is_complete_answer_payload(payload))
        .map(|(entry, payload)| (*entry, payload));
    let has_complete_payload = complete_payload.is_some();
    let selected_payload =
        complete_payload.or_else(|| payloads.last().map(|(entry, payload)| (*entry, payload)));
    let Some((answer_entry, payload)) = selected_payload else {
        return Ok(unprovenanced_trace("answer_trace.missing"));
    };

    let mut path = if has_complete_payload {
        path_from_payload(answer_entry.seq, payload)?
    } else {
        let mut partial = Vec::new();
        for (entry, payload) in &payloads {
            partial.extend(path_from_payload(entry.seq, payload)?);
            if let Some(hop) = hop_from_payload(entry.seq, payload)? {
                partial.push(hop);
            }
        }
        partial
    };
    path.sort_by_key(|hop| hop.hop);

    let kernel_entry = linked_entry(
        entries,
        payload,
        EntryKind::Kernel,
        &["kernel_id"],
        "kernel_ref",
    );
    let guard_entry = linked_entry(
        entries,
        payload,
        EntryKind::Guard,
        &["guard_id"],
        "guard_ref",
    );
    let fusion_weights = payload.get("fusion_weights").cloned();
    let guard_result = guard_entry
        .as_ref()
        .and_then(payload_value)
        .or_else(|| payload.get("guard_result").cloned());
    let freshness_ts = payload
        .get("freshness_ts")
        .or_else(|| payload.get("freshness_ts_millis"))
        .and_then(Value::as_u64);
    let mut warnings = Vec::new();
    let complete = is_complete_answer_payload(payload)
        && payload.get("path").is_some()
        && contiguous_hops(&path)
        && expected_hops_match(payload, path.len());
    if !complete {
        warnings.push(CalyxWarning::unprovenanced(
            "answer_trace.partial_or_unmarked",
        ));
    }
    if linked_payload_present(payload, &["kernel_id"], "kernel_ref") && kernel_entry.is_none() {
        warnings.push(CalyxWarning::unprovenanced(
            "answer_trace.kernel_unprovenanced",
        ));
    }
    if linked_payload_present(payload, &["guard_id"], "guard_ref") && guard_entry.is_none() {
        warnings.push(CalyxWarning::unprovenanced(
            "answer_trace.guard_unprovenanced",
        ));
    }

    Ok(AnswerTrace {
        answer_entry: Some((*answer_entry).clone()),
        kernel_entry,
        guard_entry,
        path,
        fusion_weights,
        guard_result,
        freshness_ts,
        complete,
        warnings,
    })
}

pub fn audit(
    cf_reader: &impl LedgerCfStore,
    quarantine: &dyn QuarantineLookup,
    filter: AuditFilter,
) -> Result<Vec<LedgerEntry>> {
    if let Some((start, end)) = filter.seq_range {
        ensure_range_not_quarantined(quarantine, start..end)?;
    }
    let mut out = Vec::new();
    for row in cf_reader.scan()? {
        if !filter
            .seq_range
            .is_none_or(|(start, end)| start <= row.seq && row.seq < end)
        {
            continue;
        }
        let entry = decode_physical_row(&row)?;
        if filter_matches(&entry, &filter) {
            ensure_seq_not_quarantined(quarantine, row.seq)?;
            out.push(entry);
        }
    }
    Ok(out)
}

fn decode_entries(
    cf_reader: &impl LedgerCfStore,
    quarantine: &dyn QuarantineLookup,
) -> Result<Vec<LedgerEntry>> {
    let mut entries = Vec::new();
    for row in cf_reader.scan()? {
        ensure_seq_not_quarantined(quarantine, row.seq)?;
        entries.push(decode_physical_row(&row)?);
    }
    Ok(entries)
}

fn decode_entries_from_snapshot(
    snapshot: &DecodedLedgerSnapshot,
    quarantine: &dyn QuarantineLookup,
) -> Result<Vec<LedgerEntry>> {
    let mut entries = Vec::with_capacity(snapshot.len());
    for (seq, decoded) in snapshot.rows() {
        ensure_seq_not_quarantined(quarantine, *seq)?;
        let entry = decoded.clone()?;
        if entry.seq != *seq {
            return Err(CalyxError::ledger_chain_broken(format!(
                "ledger row key {seq} does not match encoded seq {}",
                entry.seq
            )));
        }
        if !entry.verify() {
            return Err(CalyxError::ledger_corrupt(format!(
                "ledger entry seq {} hash mismatch",
                entry.seq
            )));
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn decode_physical_row(row: &LedgerRow) -> Result<LedgerEntry> {
    let entry = decode(&row.bytes)?;
    if entry.seq != row.seq {
        return Err(CalyxError::ledger_chain_broken(format!(
            "ledger row key {} does not match encoded seq {}",
            row.seq, entry.seq
        )));
    }
    Ok(entry)
}

fn filter_matches(entry: &LedgerEntry, filter: &AuditFilter) -> bool {
    filter.kind.is_none_or(|kind| entry.kind == kind)
        && filter
            .actor
            .as_ref()
            .is_none_or(|actor| &entry.actor == actor)
        && filter
            .ts_range
            .is_none_or(|(start, end)| start <= entry.ts && entry.ts < end)
        && filter
            .seq_range
            .is_none_or(|(start, end)| start <= entry.seq && entry.seq < end)
}

fn ensure_seq_not_quarantined(quarantine: &dyn QuarantineLookup, seq: u64) -> Result<()> {
    ensure_range_not_quarantined(quarantine, seq..seq.saturating_add(1))
}

fn ensure_range_not_quarantined(
    quarantine: &dyn QuarantineLookup,
    range: Range<u64>,
) -> Result<()> {
    if range.start < range.end && quarantine.contains_quarantined(range.clone())? {
        Err(CalyxError::ledger_chain_broken(format!(
            "ledger range {}..{} is quarantined",
            range.start, range.end
        )))
    } else {
        Ok(())
    }
}

fn ranges_overlap(left: &Range<u64>, right: &Range<u64>) -> bool {
    left.start < right.end && right.start < left.end
}

fn answer_subject_matches(entry: &LedgerEntry, answer_id: &[u8]) -> bool {
    matches!(&entry.subject, SubjectId::Query(bytes) if bytes == answer_id)
}

fn answer_payload(entry: &LedgerEntry) -> Result<Value> {
    serde_json::from_slice(&entry.payload)
        .map_err(|error| CalyxError::ledger_corrupt(format!("decode answer payload: {error}")))
}

fn payload_value(entry: &LedgerEntry) -> Option<Value> {
    serde_json::from_slice(&entry.payload).ok()
}

fn is_complete_answer_payload(payload: &Value) -> bool {
    payload.get("complete").and_then(Value::as_bool) == Some(true)
}

fn path_from_payload(seq: u64, payload: &Value) -> Result<Vec<AnswerTraceHop>> {
    let Some(path) = payload.get("path").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    path.iter().map(|value| trace_hop(seq, value)).collect()
}

fn hop_from_payload(seq: u64, payload: &Value) -> Result<Option<AnswerTraceHop>> {
    if payload.get("hop_index").is_some() {
        Ok(Some(trace_hop(seq, payload)?))
    } else {
        Ok(None)
    }
}

fn trace_hop(seq: u64, value: &Value) -> Result<AnswerTraceHop> {
    let hop = value
        .get("hop")
        .or_else(|| value.get("hop_index"))
        .and_then(Value::as_u64)
        .ok_or_else(|| CalyxError::ledger_corrupt("answer trace hop missing hop_index"))?;
    let cx_id = parse_cx_field(value, "cx_id")
        .or_else(|| parse_cx_field(value, "to_id"))
        .ok_or_else(|| CalyxError::ledger_corrupt("answer trace hop missing cx_id/to_id"))??;
    let from_cx_id = parse_cx_field(value, "from_id").transpose()?;
    let score = value
        .get("score")
        .or_else(|| value.get("hop_score"))
        .and_then(Value::as_f64)
        .ok_or_else(|| CalyxError::ledger_corrupt("answer trace hop missing score"))?
        as f32;
    let lens_id = parse_lens_field(value, "lens_id").transpose()?;
    let ledger_seq = value
        .get("ledger_ref")
        .and_then(|value| value.get("seq"))
        .or_else(|| value.get("ledger_seq"))
        .and_then(Value::as_u64)
        .unwrap_or(seq);
    Ok(AnswerTraceHop {
        cx_id,
        from_cx_id,
        hop: u32::try_from(hop)
            .map_err(|_| CalyxError::ledger_corrupt("answer trace hop exceeds u32"))?,
        score,
        lens_id,
        ledger_seq,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn contiguous_hops(path: &[AnswerTraceHop]) -> bool {
    path.iter()
        .enumerate()
        .all(|(index, hop)| hop.hop as usize == index)
}

fn unprovenanced_trace(surface: &str) -> AnswerTrace {
    AnswerTrace {
        answer_entry: None,
        kernel_entry: None,
        guard_entry: None,
        path: Vec::new(),
        fusion_weights: None,
        guard_result: None,
        freshness_ts: None,
        complete: false,
        warnings: vec![CalyxWarning::unprovenanced(surface)],
    }
}
