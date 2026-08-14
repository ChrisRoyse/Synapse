use std::ops::Range;

use calyx_core::{CalyxError, Result};

use crate::append::LedgerRow;
use crate::codec::{LedgerEntryRef, decode_ref_unchecked};
use crate::entry::{HASH_BYTES, compute_entry_hash_slices};
use crate::head_anchor::LedgerHeadAnchor;
use crate::verify::VerifyResult;

#[derive(Debug)]
pub enum StreamingStart {
    Ready(StreamingChainVerifier),
    Complete(VerifyResult),
}

/// How the head anchor is held against the physical row range (#2059).
///
/// `ExactHead` is the restore discipline: the vault is quiescent, so the
/// anchored head and the physical head are the same number or the vault is
/// damaged. `AnchoredPrefix` is the live discipline: rows append continuously,
/// so a row snapshot taken *after* the anchor read legitimately runs past the
/// anchored head. The anchor is then verified **mid-stream at its own height**
/// — the accumulated chain hash at `anchor.height` must equal the anchored tip
/// — which is byte-for-byte the same comparison `ExactHead` performs at the
/// end, just not raced against live appends. Rows beyond the anchor are still
/// chain-verified as the anchored tip's continuation. The one shape that stays
/// corrupt in both disciplines is an anchor *ahead* of the physical rows: the
/// caller reads the anchor before snapshotting rows, so under append-only
/// growth the anchor can trail the snapshot but can never lead it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorDiscipline {
    ExactHead,
    AnchoredPrefix,
}

#[derive(Debug)]
pub struct StreamingChainVerifier {
    range: Range<u64>,
    next_seq: u64,
    expected_prev: [u8; HASH_BYTES],
    count: u64,
    anchor: Option<LedgerHeadAnchor>,
    discipline: AnchorDiscipline,
}

impl StreamingChainVerifier {
    pub fn start(
        range: Range<u64>,
        anchor: Option<LedgerHeadAnchor>,
        previous: Option<&LedgerRow>,
        discipline: AnchorDiscipline,
    ) -> Result<StreamingStart> {
        if range.start > range.end {
            return Err(CalyxError::ledger_corrupt(format!(
                "invalid ledger range {}..{}",
                range.start, range.end
            )));
        }
        // Anchor-vs-range shape checks come BEFORE the empty-range early
        // return: an anchor claiming height N over zero physical rows is
        // corrupt in either discipline, and returning Intact for it would
        // certify a vault whose whole chain is missing (#2059).
        if range.start == 0
            && let Some(anchor) = &anchor
        {
            let mismatch = match discipline {
                AnchorDiscipline::ExactHead => range.end != anchor.height,
                // The caller reads the anchor before snapshotting rows, so an
                // anchor ahead of the snapshot cannot be explained by live
                // appends — only by a damaged chain or a damaged anchor.
                AnchorDiscipline::AnchoredPrefix => anchor.height > range.end,
            };
            if mismatch {
                return Ok(StreamingStart::Complete(corrupt_result(
                    range.end.min(anchor.height),
                    format!(
                        "ledger head anchor mismatch: requested head {}, anchored head {} (discipline={discipline:?})",
                        range.end, anchor.height
                    ),
                )));
            }
        }
        if range.start == range.end {
            return Ok(StreamingStart::Complete(VerifyResult::Intact { count: 0 }));
        }
        let expected_prev = expected_prev_hash(range.start, previous)?;
        Ok(StreamingStart::Ready(Self {
            next_seq: range.start,
            range,
            expected_prev,
            count: 0,
            anchor,
            discipline,
        }))
    }

    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub const fn end(&self) -> u64 {
        self.range.end
    }

    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Hash of the last row accepted by this verifier.
    ///
    /// The restore verifier uses this fixed-size state as its final tip instead
    /// of retaining and decoding a second copy of the last encoded row.
    pub const fn verified_tip_hash(&self) -> [u8; HASH_BYTES] {
        self.expected_prev
    }

    pub fn verify_next(&mut self, row: Option<LedgerRow>) -> Result<Option<VerifyResult>> {
        let seq = self.next_seq;
        let Some(row) = row else {
            return Ok(Some(corrupt_result(
                seq,
                format!("missing ledger row for seq {seq}"),
            )));
        };
        self.verify_next_bytes(row.seq, &row.bytes)
    }

    /// Verifies one encoded row directly from its caller-owned bytes.
    ///
    /// This is the whole-chain scrub path: parsing returns borrowed slices and
    /// canonical hashing feeds those slices incrementally, so verification does
    /// not allocate another payload/subject/actor object for every row.
    pub fn verify_next_bytes(
        &mut self,
        row_seq: u64,
        bytes: &[u8],
    ) -> Result<Option<VerifyResult>> {
        let seq = self.next_seq;
        let entry = match decode_ref_unchecked(bytes) {
            Ok(entry) => entry,
            Err(error) => {
                return Ok(Some(corrupt_result(
                    seq,
                    format!("decode ledger row seq {seq}: {error}"),
                )));
            }
        };
        if row_seq != seq || entry.seq != seq {
            return Ok(Some(corrupt_result(
                seq,
                format!("ledger key seq {seq} != encoded seq {}", entry.seq),
            )));
        }
        if entry.prev_hash != self.expected_prev {
            return Ok(Some(VerifyResult::Broken {
                at_seq: seq,
                expected: self.expected_prev,
                found: entry.prev_hash,
            }));
        }
        let expected_entry_hash = recompute_hash(&entry);
        if entry.entry_hash != expected_entry_hash {
            return Ok(Some(VerifyResult::Broken {
                at_seq: seq,
                expected: expected_entry_hash,
                found: entry.entry_hash,
            }));
        }
        self.expected_prev = entry.entry_hash;
        self.count += 1;
        self.next_seq += 1;
        // #2059 AnchoredPrefix: the anchor is verified at its OWN height as
        // the stream passes it — the accumulated tip after rows [0..height)
        // must equal the anchored tip. This is the identical comparison
        // ExactHead makes in finish(), performed where the anchor actually
        // points instead of raced against rows appended after the anchor was
        // read. Fires equally when anchor.height == range.end (the comparison
        // happens before the finish() below returns Intact).
        if self.discipline == AnchorDiscipline::AnchoredPrefix
            && self.range.start == 0
            && let Some(anchor) = &self.anchor
            && self.next_seq == anchor.height
            && self.expected_prev != anchor.tip_hash
        {
            return Ok(Some(VerifyResult::Broken {
                at_seq: anchor.height.saturating_sub(1),
                expected: anchor.tip_hash,
                found: self.expected_prev,
            }));
        }
        Ok((self.next_seq == self.range.end).then(|| self.finish()))
    }

    fn finish(&self) -> VerifyResult {
        if self.discipline == AnchorDiscipline::ExactHead
            && self.range.start == 0
            && let Some(anchor) = &self.anchor
            && self.range.end == anchor.height
            && self.expected_prev != anchor.tip_hash
        {
            return VerifyResult::Broken {
                at_seq: self.range.end.saturating_sub(1),
                expected: anchor.tip_hash,
                found: self.expected_prev,
            };
        }
        VerifyResult::Intact { count: self.count }
    }
}

fn expected_prev_hash(start: u64, previous: Option<&LedgerRow>) -> Result<[u8; HASH_BYTES]> {
    if start == 0 {
        return Ok([0; HASH_BYTES]);
    }
    let previous_seq = start - 1;
    let Some(row) = previous else {
        return Err(CalyxError::ledger_corrupt(format!(
            "missing ledger row for previous seq {previous_seq}"
        )));
    };
    let entry = decode_ref_unchecked(&row.bytes).map_err(|error| {
        CalyxError::ledger_corrupt(format!(
            "cannot verify range start {start}: previous seq {previous_seq}: {error}"
        ))
    })?;
    if row.seq != previous_seq || entry.seq != previous_seq {
        return Err(CalyxError::ledger_corrupt(format!(
            "previous key seq {previous_seq} != encoded seq {}",
            entry.seq
        )));
    }
    if entry.entry_hash != recompute_hash(&entry) {
        return Err(CalyxError::ledger_corrupt(format!(
            "cannot verify range start {start}: previous seq {previous_seq} is broken"
        )));
    }
    Ok(entry.entry_hash)
}

fn recompute_hash(entry: &LedgerEntryRef<'_>) -> [u8; HASH_BYTES] {
    compute_entry_hash_slices(
        entry.seq,
        &entry.prev_hash,
        entry.kind,
        entry.subject_tag,
        entry.subject_bytes,
        entry.payload,
        entry.actor_tag,
        entry.actor_bytes,
        entry.ts,
    )
}

fn corrupt_result(at_seq: u64, reason: impl Into<String>) -> VerifyResult {
    VerifyResult::Corrupt {
        at_seq,
        reason: reason.into(),
    }
}
