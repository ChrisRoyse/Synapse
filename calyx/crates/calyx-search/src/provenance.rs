use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use calyx_aster::ledger_view::read_ledger_seqs_traced;
use calyx_aster::mvcc::Snapshot;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Constellation, CxId, LedgerRef};
use calyx_ledger::{EntryKind, LedgerEntry, SubjectId, decode};
use calyx_sextant::{
    CALYX_SEXTANT_PROVENANCE_MISSING, CALYX_SEXTANT_PROVENANCE_SHAPE_UNREGISTERED,
    CALYX_SEXTANT_PROVENANCE_SUBJECT_UNRESOLVED, FreshnessTag, Hit, ProvenanceSource,
    sextant_error,
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
            hit.provenance = verifier.require_ref(hit.cx_id, cx.provenance.clone(), trace)?;
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

    fn require_ref(
        &mut self,
        cx_id: CxId,
        expected: LedgerRef,
        trace: &mut crate::engine_trace::SearchTracer<'_>,
    ) -> CliResult<LedgerRef> {
        let entry = self.entry(cx_id, expected.seq)?;
        let entry_hash = entry.entry_hash;
        let entry_kind = entry.kind;
        let subject = SubjectShape::of(&entry.subject);
        if entry.entry_hash != expected.hash {
            return Err(CalyxError::ledger_corrupt(format!(
                "search hit {cx_id} ledger seq {} hash does not match Base provenance",
                expected.seq
            ))
            .into());
        }
        // Past this point the ledger row is byte-for-byte the one Base recorded,
        // so nothing below may report corruption on the strength of coverage
        // alone (#2084). Coverage answers a different question — does this entry
        // attest THIS constellation — and its negatives are provenance faults,
        // not vault damage.
        match entry_coverage(entry, cx_id)? {
            Coverage::Subject | Coverage::PayloadList => {}
            Coverage::BatchScope => {
                // The entry is a declared batch attestation that does not
                // enumerate its members, so coverage rests on the entry-hash and
                // chain-link binding above rather than on an identity carried in
                // the entry. Say so in the trace instead of silently presenting
                // it as a subject-verified hit.
                trace.emit_detail(
                    "provenance.ledger_coverage.batch_scoped",
                    None,
                    None,
                    Some(format!(
                        "cx={cx_id} seq={} kind={entry_kind} subject={}",
                        expected.seq,
                        subject.tag()
                    )),
                );
            }
            Coverage::NotCovered => {
                return Err(sextant_error(
                    CALYX_SEXTANT_PROVENANCE_SUBJECT_UNRESOLVED,
                    format!(
                        "search hit {cx_id} ledger seq {} carries the {entry_kind}/{} shape, whose \
                         subject names a different constellation and whose payload does not list \
                         this one; the entry hash matches the stored Base provenance, so the \
                         ledger row is intact",
                        expected.seq,
                        subject.tag()
                    ),
                )
                .into());
            }
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

/// The `SubjectId` variant, stripped of its identity payload so the coverage
/// table can dispatch on shape.
///
/// The conversion is an exhaustive `match` on purpose: a subject variant added
/// to `calyx-ledger` fails this build instead of silently collapsing into an
/// existing arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubjectShape {
    Cx,
    Lens,
    Kernel,
    Guard,
    Query,
}

impl SubjectShape {
    fn of(subject: &SubjectId) -> Self {
        match subject {
            SubjectId::Cx(_) => Self::Cx,
            SubjectId::Lens(_) => Self::Lens,
            SubjectId::Kernel(_) => Self::Kernel,
            SubjectId::Guard(_) => Self::Guard,
            SubjectId::Query(_) => Self::Query,
        }
    }

    /// Stable label carried in evidence so a refusal names the shape it saw.
    fn tag(self) -> &'static str {
        match self {
            Self::Cx => "cx",
            Self::Lens => "lens",
            Self::Kernel => "kernel",
            Self::Guard => "guard",
            Self::Query => "query",
        }
    }
}

/// How one declared ledger-entry shape names the constellations it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoverageRule {
    /// The entry covers exactly the constellation named by its `Cx` subject, so
    /// a `Cx` subject naming a different constellation is a decided negative.
    SubjectCx,
    /// A batch entry that enumerates its members in the payload under `cx_id`.
    /// A payload that cannot be read as that shape is malformed — a genuine
    /// corruption signal, not a negative.
    EnumeratedCxList,
    /// A declared batch entry that stamps one `LedgerRef` onto every Base row it
    /// commits without naming any of them in the entry. Membership is not
    /// decidable from the entry; the Base row's binding to it is established by
    /// the entry-hash and chain-link checks that run before coverage.
    BatchScoped,
    /// No writer in this workspace stamps a Base row's provenance with this
    /// shape. Fail closed naming the shape rather than guess at its membership.
    Unregistered,
}

/// The declared coverage rule for one `(EntryKind, SubjectShape)` shape.
///
/// #2084: coverage recognition used to be two hard-coded special cases — a `Cx`
/// subject naming the hit, or an `Ingest` entry whose payload listed it — and
/// every other legitimately minted shape fell through to a bare `false`, which
/// the caller reported as `CALYX_LEDGER_CORRUPT` with a "restore from restic"
/// remediation, on rows whose entry hash had just matched their Base provenance
/// byte for byte. The concrete shape that tripped it is the multi-constellation
/// grounding anchor batch (`Grounding` / `Query`), which stamps one entry onto
/// every constellation in the batch.
///
/// Recognition is now this declared table, and both matches below are exhaustive
/// — over `EntryKind` and over `SubjectShape` — so a kind or subject variant
/// added to `calyx-ledger` breaks this build rather than degrading into a false
/// corruption verdict at query time. Each arm cites the writer that mints it.
fn coverage_rule(kind: EntryKind, subject: SubjectShape) -> CoverageRule {
    match kind {
        // Single ingest stamps `Cx(cx)` (calyx-aster vault/store.rs,
        // vault/dedup_commit.rs, vault/ledger_append.rs
        // `stage_raw_ingest_ledger_locked`) and is decided by the subject.
        // Batch ingest names only the FIRST accepted constellation as subject
        // and enumerates the whole batch in the payload `cx_id` array
        // (calyx-aster vault/batch_ingest.rs `batch_payload`), so a `Cx` subject
        // that is not ours must enumerate. Every other subject variant is an
        // opaque batch marker minted by a caller supplying its own subject and
        // payload: the derived snapshot publication
        // (`Query(operation_id)`, synapse-calyx panel_lifecycle.rs), the
        // cross-model transaction (`Query(batch digest)`, calyx-aster
        // txn/cross_model.rs), the precondition-gated ingest
        // (`Guard(vault_id)`, calyx-aster vault/ingest_precondition.rs) and the
        // KV/document/relational/timeseries/blob layer commits. None of those
        // enumerate their members.
        EntryKind::Ingest => match subject {
            SubjectShape::Cx => CoverageRule::EnumeratedCxList,
            SubjectShape::Lens
            | SubjectShape::Kernel
            | SubjectShape::Guard
            | SubjectShape::Query => CoverageRule::BatchScoped,
        },
        // The single-anchor grounding writer stamps `Cx(cx)` (synapse-calyx
        // `put_grounding_anchors`). The multi-constellation writer stamps ONE
        // `Query(b"synapse.grounding_anchor.multi.v1")` entry onto every
        // constellation in the batch (synapse-calyx
        // `put_grounding_anchors_for_many` -> calyx-aster
        // vault/ledger_anchor_batch.rs `multi_cx_anchor_rows_with_ledger_ref`)
        // and its payload lists hashed source-row keys, never cx ids. That is
        // the shape #2084 reported as vault corruption.
        EntryKind::Grounding => match subject {
            SubjectShape::Cx => CoverageRule::SubjectCx,
            SubjectShape::Query => CoverageRule::BatchScoped,
            SubjectShape::Lens | SubjectShape::Kernel | SubjectShape::Guard => {
                CoverageRule::Unregistered
            }
        },
        // Single-subject rewrites of one stored constellation:
        //   Migrate  — retained-input-pointer and temporal-metadata backfills
        //              (calyx-aster vault/input_pointer.rs, vault/temporal_metadata.rs)
        //   Guard    — reactive trigger persistence and Ward verdicts
        //              (calyx-loom reactive/durable.rs, calyx-ward ledger.rs)
        //   Admin    — GC orphan Base repair (calyx-aster gc/orphan_reconciler)
        //   Erase    — per-constellation tombstones (calyx-aster erase.rs)
        // Each names its constellation in the subject and never covers a second
        // one, so a different `Cx` subject is a decided negative. Their non-`Cx`
        // forms (vault-wide erase `Guard(scope digest)`, residency/retention
        // `Guard(..)`, reproduce and checkpoint `Query(..)`, subscription
        // `Guard(..)`) commit no Base rows at all.
        EntryKind::Migrate | EntryKind::Guard | EntryKind::Admin | EntryKind::Erase => {
            match subject {
                SubjectShape::Cx => CoverageRule::SubjectCx,
                SubjectShape::Lens
                | SubjectShape::Kernel
                | SubjectShape::Guard
                | SubjectShape::Query => CoverageRule::Unregistered,
            }
        }
        // Kinds with no Base-stamping writer in the workspace. Measure, Score,
        // Admission and AgentForecast have no writer at all; Assay, Kernel,
        // Answer, Anneal, Policy and BatchCommitment write evidence rows
        // (Registry/Graph/AnnealReport/checkpoint cohorts) and never rewrite a
        // Base row's provenance. A `Cx` subject naming the hit is still accepted
        // before this table is consulted, so reaching here means the entry
        // neither names the hit nor belongs to a declared batch shape.
        EntryKind::Measure
        | EntryKind::Assay
        | EntryKind::Kernel
        | EntryKind::Answer
        | EntryKind::Anneal
        | EntryKind::Admission
        | EntryKind::AgentForecast
        | EntryKind::Policy
        | EntryKind::Score
        | EntryKind::BatchCommitment => match subject {
            SubjectShape::Cx
            | SubjectShape::Lens
            | SubjectShape::Kernel
            | SubjectShape::Guard
            | SubjectShape::Query => CoverageRule::Unregistered,
        },
    }
}

/// The decided relationship between one ledger entry and one constellation.
///
/// Separating these from "the entry is unreadable" is the whole point of #2084:
/// only a malformed entry is a corruption signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coverage {
    /// The entry's `Cx` subject names this constellation.
    Subject,
    /// The entry's payload member list names this constellation.
    PayloadList,
    /// A declared batch shape whose membership the entry does not enumerate.
    BatchScope,
    /// A recognised shape that positively does not cover this constellation.
    NotCovered,
}

fn entry_coverage(entry: &LedgerEntry, cx_id: CxId) -> CliResult<Coverage> {
    if entry.subject == SubjectId::Cx(cx_id) {
        return Ok(Coverage::Subject);
    }
    let subject = SubjectShape::of(&entry.subject);
    match coverage_rule(entry.kind, subject) {
        CoverageRule::SubjectCx => Ok(Coverage::NotCovered),
        CoverageRule::EnumeratedCxList => {
            if payload_names_cx(entry, cx_id)? {
                Ok(Coverage::PayloadList)
            } else {
                Ok(Coverage::NotCovered)
            }
        }
        CoverageRule::BatchScoped => Ok(Coverage::BatchScope),
        CoverageRule::Unregistered => Err(sextant_error(
            CALYX_SEXTANT_PROVENANCE_SHAPE_UNREGISTERED,
            format!(
                "search hit {cx_id} ledger seq {} carries the {}/{} shape, which no declared \
                 writer stamps onto a Base row's provenance; the entry hash matches the stored \
                 Base provenance, so the ledger row itself is intact",
                entry.seq,
                entry.kind,
                subject.tag()
            ),
        )
        .into()),
    }
}

/// Reads the `cx_id` member declaration of an enumerating batch entry.
///
/// Both observed conventions are accepted: the single-constellation writers
/// carry `cx_id` as a string, batch ingest carries it as an array. Anything else
/// is a malformed entry of a shape declared to enumerate — the one condition in
/// this file that is genuinely a ledger-content defect, so it keeps the loud
/// corruption verdict and its restore remediation.
fn payload_names_cx(entry: &LedgerEntry, cx_id: CxId) -> CliResult<bool> {
    let payload = serde_json::from_slice::<Value>(&entry.payload).map_err(|error| {
        CalyxError::ledger_corrupt(format!(
            "ledger seq {} carries the {} shape, declared to enumerate its constellations, but \
             its payload is not valid JSON: {error}",
            entry.seq, entry.kind
        ))
    })?;
    let members = payload.get("cx_id").ok_or_else(|| {
        CalyxError::ledger_corrupt(format!(
            "ledger seq {} carries the {} shape, declared to enumerate its constellations, but \
             its payload carries no cx_id member declaration",
            entry.seq, entry.kind
        ))
    })?;
    let target = cx_id.to_string();
    match members {
        Value::String(single) => Ok(single == &target),
        Value::Array(ids) => Ok(ids
            .iter()
            .any(|value| value.as_str() == Some(target.as_str()))),
        other => Err(CalyxError::ledger_corrupt(format!(
            "ledger seq {} carries the {} shape, but its payload cx_id member declaration is \
             neither a constellation id nor a list of them ({})",
            entry.seq,
            entry.kind,
            json_type_name(other)
        ))
        .into()),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
