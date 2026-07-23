use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use calyx_aster::ledger_view::read_ledger_seqs_traced;
use calyx_aster::mvcc::Snapshot;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Constellation, CxId, LedgerRef};
use calyx_ledger::{EntryKind, LedgerEntry, SubjectId, decode};
use calyx_sextant::{
    CALYX_SEXTANT_PROVENANCE_MISSING, FreshnessTag, Hit, ProvenanceSource, sextant_error,
};
use serde_json::Value;

use crate::error::CliResult;

pub(crate) fn hit_docs_at<C: Clock>(
    vault: &AsterVault<C>,
    hits: &[Hit],
    snapshot: Snapshot,
    hydrate_slots: bool,
) -> CliResult<BTreeMap<CxId, Constellation>> {
    let mut docs = BTreeMap::new();
    for hit in hits {
        let cx_id = hit.cx_id;
        let read = if hydrate_slots {
            let required_slots = hit
                .per_lens
                .iter()
                .map(|lens_hit| lens_hit.slot.slot_id())
                .collect::<BTreeSet<_>>();
            vault.get_selected_slots_at_snapshot(cx_id, snapshot, required_slots)
        } else {
            vault.get_base_at_snapshot(cx_id, snapshot)
        };
        let cx = read.map_err(|error| {
            if error.code == "CALYX_STALE_DERIVED" && error.message.contains("missing") {
                missing_provenance(format!("stored constellation missing for hit {cx_id}"))
            } else {
                error
            }
        })?;
        if let Some(mismatched) = hit
            .per_lens
            .iter()
            .find(|lens_hit| lens_hit.slot.panel_version() != cx.panel_version)
        {
            return Err(CalyxError::stale_derived(format!(
                "hit {cx_id} from panel {} contains contribution from {}",
                cx.panel_version, mismatched.slot
            ))
            .into());
        }
        docs.insert(cx_id, cx);
    }
    Ok(docs)
}

pub(crate) fn attach_verified_provenance(
    hits: &mut [Hit],
    docs: &BTreeMap<CxId, Constellation>,
    vault_dir: &Path,
    freshness: FreshnessTag,
    trace: &mut crate::engine_trace::SearchTracer<'_>,
) -> CliResult {
    let vault_key = crate::persisted::canonical_pin_vault_dir(vault_dir)?;
    // Freeze each hit's memo decision NOW: verifying pending hits inserts
    // into the bounded process-global memo, which can evict another hit's
    // entry mid-loop. Re-querying the memo during the serve loop would then
    // route that hit to a verifier that never loaded its ledger seqs — a
    // spurious missing-ledger-seq failure (or a panic when nothing was
    // pending). The verifier is opened for exactly the !memoized hits below.
    let memoized = hits
        .iter()
        .map(|hit| {
            docs.get(&hit.cx_id)
                .is_some_and(|cx| ledger_memo_contains(&vault_key, hit.cx_id, &cx.provenance))
        })
        .collect::<Vec<_>>();
    let pending = hits
        .iter()
        .zip(&memoized)
        .filter(|(_, hit_memoized)| !**hit_memoized)
        .map(|(hit, _)| hit.clone())
        .collect::<Vec<_>>();
    let mut ledger = if pending.is_empty() {
        None
    } else {
        Some(TargetedLedgerVerifier::open(
            vault_dir, &pending, docs, trace,
        )?)
    };
    for (hit, hit_memoized) in hits.iter_mut().zip(memoized) {
        let cx = docs.get(&hit.cx_id).ok_or_else(|| {
            missing_provenance(format!(
                "stored constellation missing for hit {}",
                hit.cx_id
            ))
        })?;
        if hit_memoized {
            // The exact (cx_id, ledger seq, entry hash) triple already passed
            // the targeted ledger verification in this process; the ledger is
            // append-only, so the verification result is immutable.
            hit.provenance = cx.provenance.clone();
        } else {
            let verifier = ledger
                .as_mut()
                .expect("pending hits imply an opened ledger verifier");
            hit.provenance = verifier.require_ref(hit.cx_id, cx.provenance.clone())?;
            ledger_memo_insert(&vault_key, hit.cx_id, &cx.provenance);
        }
        hit.provenance_source = ProvenanceSource::Stored;
        hit.freshness = freshness.clone();
    }
    Ok(())
}

const MAX_MEMOIZED_LEDGER_REFS: usize = 8192;

type LedgerRefKey = (String, CxId, u64, [u8; 32]);

struct LedgerRefMemo {
    verified: BTreeSet<LedgerRefKey>,
    order: VecDeque<LedgerRefKey>,
}

fn ledger_memo() -> &'static Mutex<LedgerRefMemo> {
    static MEMO: OnceLock<Mutex<LedgerRefMemo>> = OnceLock::new();
    MEMO.get_or_init(|| {
        Mutex::new(LedgerRefMemo {
            verified: BTreeSet::new(),
            order: VecDeque::new(),
        })
    })
}

fn ledger_memo_contains(vault_key: &str, cx_id: CxId, provenance: &LedgerRef) -> bool {
    let key = (
        vault_key.to_string(),
        cx_id,
        provenance.seq,
        provenance.hash,
    );
    ledger_memo()
        .lock()
        .expect("ledger ref memo poisoned")
        .verified
        .contains(&key)
}

fn ledger_memo_insert(vault_key: &str, cx_id: CxId, provenance: &LedgerRef) {
    let key = (
        vault_key.to_string(),
        cx_id,
        provenance.seq,
        provenance.hash,
    );
    let mut memo = ledger_memo().lock().expect("ledger ref memo poisoned");
    if memo.verified.insert(key.clone()) {
        memo.order.push_back(key);
    }
    while memo.order.len() > MAX_MEMOIZED_LEDGER_REFS {
        if let Some(evicted) = memo.order.pop_front() {
            memo.verified.remove(&evicted);
        }
    }
}

struct TargetedLedgerVerifier {
    rows: BTreeMap<u64, calyx_ledger::LedgerRow>,
    entries: BTreeMap<u64, LedgerEntry>,
}

impl TargetedLedgerVerifier {
    fn open(
        vault_dir: &Path,
        hits: &[Hit],
        docs: &BTreeMap<CxId, Constellation>,
        trace: &mut crate::engine_trace::SearchTracer<'_>,
    ) -> CliResult<Self> {
        let mut required = BTreeSet::new();
        for hit in hits {
            let cx = docs.get(&hit.cx_id).ok_or_else(|| {
                missing_provenance(format!(
                    "stored constellation missing for hit {}",
                    hit.cx_id
                ))
            })?;
            required.insert(cx.provenance.seq);
            if cx.provenance.seq > 0 {
                required.insert(cx.provenance.seq - 1);
            }
        }
        let (rows, point_read) = read_ledger_seqs_traced(vault_dir, &required)?;
        // Structured tier attribution (#1112): one event per point-read tier
        // so FSV can assert from the runtime log which tier resolved the
        // targeted ledger seqs and that the complete-SST scan never ran.
        for tier in &point_read.tiers {
            trace.emit_detail(
                "provenance.ledger_point_read.tier",
                None,
                Some(tier.resolved),
                Some(format!(
                    "tier={} wanted={} resolved={} files_opened={} tier_elapsed_ms={}",
                    tier.tier, tier.wanted, tier.resolved, tier.files_opened, tier.elapsed_ms
                )),
            );
        }
        Ok(Self {
            rows,
            entries: BTreeMap::new(),
        })
    }

    fn require_ref(&mut self, cx_id: CxId, expected: LedgerRef) -> CliResult<LedgerRef> {
        let entry = self.entry(cx_id, expected.seq)?;
        let entry_hash = entry.entry_hash;
        if entry.entry_hash != expected.hash {
            return Err(CalyxError::ledger_corrupt(format!(
                "search hit {cx_id} ledger seq {} hash does not match Base provenance",
                expected.seq
            ))
            .into());
        }
        if !entry_covers_cx(entry, cx_id)? {
            return Err(CalyxError::ledger_corrupt(format!(
                "search hit {cx_id} ledger seq {} subject mismatch",
                expected.seq
            ))
            .into());
        }
        self.require_chain_link(cx_id, expected.seq, entry_hash)?;
        Ok(expected)
    }

    fn entry(&mut self, cx_id: CxId, seq: u64) -> CliResult<&LedgerEntry> {
        if !self.entries.contains_key(&seq) {
            let bytes = self
                .rows
                .get(&seq)
                .ok_or_else(|| {
                    missing_provenance(format!(
                        "search hit {cx_id} references missing ledger seq {seq}"
                    ))
                })?
                .clone()
                .bytes;
            let entry = decode(&bytes).map_err(|error| {
                CalyxError::ledger_chain_broken(format!(
                    "search hit {cx_id} ledger seq {seq} is unreadable: {}",
                    error.message
                ))
            })?;
            if entry.seq != seq {
                return Err(CalyxError::ledger_corrupt(format!(
                    "search hit {cx_id} ledger row decoded seq {} != requested seq {seq}",
                    entry.seq
                ))
                .into());
            }
            self.entries.insert(seq, entry);
        }
        Ok(self
            .entries
            .get(&seq)
            .expect("targeted ledger entry inserted before lookup"))
    }

    fn require_chain_link(&mut self, cx_id: CxId, seq: u64, entry_hash: [u8; 32]) -> CliResult {
        if seq == 0 {
            let entry = self.entry(cx_id, seq)?;
            if entry.prev_hash != [0; 32] {
                return Err(CalyxError::ledger_chain_broken(format!(
                    "search hit {cx_id} ledger seq 0 prev_hash is not the genesis hash"
                ))
                .into());
            }
            return Ok(());
        }
        let previous = self.entry(cx_id, seq - 1)?;
        let previous_hash = previous.entry_hash;
        let entry = self.entry(cx_id, seq)?;
        if entry.prev_hash != previous_hash {
            return Err(CalyxError::ledger_chain_broken(format!(
                "search hit {cx_id} ledger seq {seq} prev_hash does not match seq {} entry_hash",
                seq - 1
            ))
            .into());
        }
        if entry.entry_hash != entry_hash {
            return Err(CalyxError::ledger_chain_broken(format!(
                "search hit {cx_id} ledger seq {seq} changed during targeted verification"
            ))
            .into());
        }
        Ok(())
    }
}

fn missing_provenance(message: impl Into<String>) -> CalyxError {
    sextant_error(CALYX_SEXTANT_PROVENANCE_MISSING, message)
}

fn entry_covers_cx(entry: &LedgerEntry, cx_id: CxId) -> CliResult<bool> {
    if entry.subject == SubjectId::Cx(cx_id) {
        return Ok(true);
    }
    if entry.kind != EntryKind::Ingest {
        return Ok(false);
    }
    batch_ingest_payload_contains_cx(entry, cx_id)
}

fn batch_ingest_payload_contains_cx(entry: &LedgerEntry, cx_id: CxId) -> CliResult<bool> {
    let payload = serde_json::from_slice::<Value>(&entry.payload).map_err(|error| {
        CalyxError::ledger_corrupt(format!(
            "ingest ledger seq {} subject mismatch and payload is invalid JSON: {error}",
            entry.seq
        ))
    })?;
    let ids = payload
        .get("cx_id")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CalyxError::ledger_corrupt(format!(
                "ingest ledger seq {} subject mismatch and payload missing cx_id array",
                entry.seq
            ))
        })?;
    Ok(ids
        .iter()
        .any(|value| value.as_str() == Some(&cx_id.to_string())))
}
