use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use calyx_aster::ledger_view::read_ledger_seqs_traced;
use calyx_aster::mvcc::Snapshot;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Constellation, CxId, LedgerRef};
use calyx_ledger::{
    CxCoverage, LedgerEntry, SubjectShape, decode, entry_cx_coverage, read_batch_members,
};
use calyx_sextant::{
    CALYX_SEXTANT_PROVENANCE_MISSING, CALYX_SEXTANT_PROVENANCE_SHAPE_UNREGISTERED,
    CALYX_SEXTANT_PROVENANCE_SUBJECT_UNRESOLVED, FreshnessTag, Hit, ProvenanceSource,
    sextant_error,
};

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
        let coverage = entry_coverage(entry, cx_id)?;
        match coverage {
            Coverage::Subject | Coverage::PayloadList => {}
            Coverage::BatchMemberList => {
                // #2096: a batch entry that enumerates its members. Coverage is
                // decided FROM the entry, not merely consistent with it, so the
                // trace records the stronger fact separately — an FSV that
                // cannot tell these two apart cannot prove the tightening.
                trace.emit_detail(
                    "provenance.ledger_coverage.batch_verified",
                    None,
                    Some(1),
                    Some(format!(
                        "cx={cx_id} seq={} kind={entry_kind} subject={} members={}",
                        expected.seq,
                        subject.tag(),
                        read_batch_members(&entry.payload)?
                            .total()
                            .map_or_else(|| "?".to_owned(), |total| total.to_string()),
                    )),
                );
            }
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
    /// A batch entry's versioned `batch_members` declaration names this
    /// constellation, so a batch shape is positively verified rather than
    /// merely accepted (#2096).
    BatchMemberList,
    /// A declared batch shape whose membership the entry does not enumerate.
    BatchScope,
    /// A recognised shape that positively does not cover this constellation.
    NotCovered,
}

/// Decides one ledger entry against one constellation.
///
/// # Where the rules live (#2095)
///
/// The `(EntryKind, SubjectShape)` table this dispatches on used to live in this
/// file, which meant the writer that stamped a Base row and the reader that
/// judged it were free to disagree — and did, which is #2084. It now lives in
/// [`calyx_ledger::base_stamp`], the crate both sides already depend on, and
/// `calyx-aster` refuses at WRITE time any pair this table would refuse here.
/// The two are the same declaration, not two copies of one.
///
/// # Why batch entries are no longer merely trusted (#2096)
///
/// A `BatchScoped` shape stamps one entry onto every Base row it commits. Before
/// #2096 none of them named a member, so acceptance rested entirely on the
/// entry-hash and chain-link binding checked before coverage — good enough to
/// know the row points at an intact, unaltered entry, but not enough to *prove*
/// that entry attests this row. Entries minted after #2096 carry a
/// [`calyx_ledger::batch_members`] declaration, and where one is present it is
/// authoritative in both directions: a listed hit is positively verified, and a
/// hit absent from a COMPLETE list is a decided negative.
///
/// Historical entries carry no declaration and keep the trusting path exactly as
/// before — an append-only ledger cannot be upgraded in place, so re-judging old
/// rows under a rule they predate would refuse hits on evidence that was never
/// required of their writers. A truncated declaration is likewise never read as
/// a negative: absence from a bounded prefix proves nothing.
fn entry_coverage(entry: &LedgerEntry, cx_id: CxId) -> CliResult<Coverage> {
    let coverage = entry_cx_coverage(entry, cx_id).map_err(|error| {
        sextant_error(
            CALYX_SEXTANT_PROVENANCE_SHAPE_UNREGISTERED,
            format!(
                "search hit {cx_id} cannot resolve ledger seq {} {}/{} coverage through the \
                 shared Base-row contract ({}: {}); the entry hash matches the stored Base \
                 provenance, so the ledger row itself is intact",
                entry.seq,
                entry.kind,
                SubjectShape::of(&entry.subject).tag(),
                error.code,
                error.message
            ),
        )
    })?;
    Ok(match coverage {
        CxCoverage::Subject => Coverage::Subject,
        CxCoverage::EnumeratedMember => Coverage::PayloadList,
        CxCoverage::BatchMember => Coverage::BatchMemberList,
        CxCoverage::BatchScope => Coverage::BatchScope,
        CxCoverage::NotCovered => Coverage::NotCovered,
    })
}
