//! Append-only ledger writer and row-store adapters.

mod memory;

use std::borrow::Cow;
use std::collections::BTreeMap;

use calyx_core::{CalyxError, Clock, LedgerRef, Result};

use crate::codec::{decode, encode};
use crate::entry::{ActorId, HASH_BYTES, LedgerEntry, SubjectId};
use crate::head_anchor::{LedgerHeadAnchor, verify_recovered_tip};
use crate::kind::EntryKind;
use crate::redaction::RedactionPolicy;

pub use crate::directory_store::DirectoryLedgerStore;
pub use memory::MemoryLedgerStore;

/// Physical ledger row keyed by big-endian sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerRow {
    pub seq: u64,
    pub bytes: Vec<u8>,
}

/// One coherent read view of ledger rows and their external head witness.
///
/// Stores that already own an immutable in-memory view can return a borrowed
/// snapshot. Other implementations receive an owned snapshot from the trait
/// default. Consumers can then count, verify, and audit the same rows without
/// re-entering [`LedgerCfStore::scan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerSnapshot<'a> {
    rows: Cow<'a, [LedgerRow]>,
    head_anchor: Option<Cow<'a, LedgerHeadAnchor>>,
}

impl<'a> LedgerSnapshot<'a> {
    pub fn borrowed(rows: &'a [LedgerRow], head_anchor: Option<&'a LedgerHeadAnchor>) -> Self {
        Self {
            rows: Cow::Borrowed(rows),
            head_anchor: head_anchor.map(Cow::Borrowed),
        }
    }

    pub fn owned(rows: Vec<LedgerRow>, head_anchor: Option<LedgerHeadAnchor>) -> Self {
        Self {
            rows: Cow::Owned(rows),
            head_anchor: head_anchor.map(Cow::Owned),
        }
    }

    pub fn rows(&self) -> &[LedgerRow] {
        &self.rows
    }

    pub fn head_anchor(&self) -> Option<&LedgerHeadAnchor> {
        self.head_anchor.as_deref()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedLedgerEntry {
    entry: LedgerEntry,
    bytes: Vec<u8>,
}

impl PreparedLedgerEntry {
    pub const fn seq(&self) -> u64 {
        self.entry.seq
    }

    pub const fn entry_hash(&self) -> [u8; HASH_BYTES] {
        self.entry.entry_hash
    }

    pub const fn prev_hash(&self) -> [u8; HASH_BYTES] {
        self.entry.prev_hash
    }

    pub const fn ts(&self) -> u64 {
        self.entry.ts
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn ledger_ref(&self) -> LedgerRef {
        LedgerRef {
            seq: self.entry.seq,
            hash: self.entry.entry_hash,
        }
    }
}

/// Minimal append-only `ledger` CF contract used by `LedgerAppender`.
///
/// The `scan`-backed default [`read_seq`](Self::read_seq) and absent
/// [`head_anchor`](Self::head_anchor) are correctness-only defaults for tiny
/// stores and tests. Any durable or non-trivial store must override
/// `read_seq`, `head_anchor`, and `put_head_anchor` so appends recover from a
/// monotonic head witness instead of decode-scanning the full ledger on every
/// append.
pub trait LedgerCfStore {
    /// Returns all rows sorted by sequence number.
    fn scan(&self) -> Result<Vec<LedgerRow>>;

    /// Acquires rows and the external head witness for one logical read.
    ///
    /// Implementations backed by a shared lock or immutable resident view
    /// should override this method so both values come from the same guarded
    /// generation and so the rows can be borrowed rather than copied.
    fn snapshot(&self) -> Result<LedgerSnapshot<'_>> {
        let head_anchor = self.head_anchor()?;
        let rows = self.scan()?;
        Ok(LedgerSnapshot::owned(rows, head_anchor))
    }

    /// Returns at most the newest `n` rows sorted by sequence number.
    ///
    /// The default full-scans [`scan`](Self::scan). Stores with ordered keys
    /// should override this so status/readback paths do not decode the whole
    /// append-only ledger for small recent windows.
    fn scan_recent(&self, n: usize) -> Result<Vec<LedgerRow>> {
        if n == usize::MAX {
            return self.scan();
        }
        let mut rows = self.scan()?;
        if n < rows.len() {
            rows.drain(0..rows.len() - n);
        }
        Ok(rows)
    }

    /// Returns at most the newest `n` rows of `kind`, sorted by sequence number.
    ///
    /// The default performs one complete scan. Ordered stores with a derived
    /// kind index should override this so a sparse or absent kind does not turn
    /// a recent-history read into repeated full-ledger scans.
    fn scan_recent_by_kind(&self, kind: EntryKind, n: usize) -> Result<Vec<LedgerRow>> {
        if n == 0 {
            return Ok(Vec::new());
        }
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
        Ok(rows)
    }

    /// Reads one ledger row by sequence number.
    ///
    /// The default full-scans [`scan`](Self::scan). Override this for any store
    /// that can grow beyond a small deterministic fixture.
    fn read_seq(&self, seq: u64) -> Result<Option<LedgerRow>> {
        Ok(self.scan()?.into_iter().find(|row| row.seq == seq))
    }

    /// Writes a new row. Implementations must reject overwrites.
    fn put_new(&mut self, seq: u64, bytes: &[u8]) -> Result<()>;

    /// Rejects delete paths for the ledger CF.
    fn delete(&mut self, seq: u64) -> Result<()> {
        reject_delete(seq)
    }

    /// Rejects tombstone paths for the ledger CF.
    fn tombstone(&mut self, seq: u64) -> Result<()> {
        reject_tombstone(seq)
    }

    /// External monotonic head witness, if this store has one.
    ///
    /// Durable stores must override this and [`put_head_anchor`](Self::put_head_anchor).
    /// Returning `None` forces recovery through a complete ledger scan.
    fn head_anchor(&self) -> Result<Option<LedgerHeadAnchor>> {
        Ok(None)
    }

    /// Persists the newest committed head witness.
    ///
    /// Durable stores must persist this witness; the default no-op is only for
    /// scan-only test stores.
    fn put_head_anchor(&mut self, _anchor: &LedgerHeadAnchor) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreparePosition {
    seq: u64,
    prev_hash: [u8; HASH_BYTES],
    last_ts: u64,
}

/// The single write path for the hash-chained append-only ledger.
#[derive(Debug)]
pub struct LedgerAppender<S, C> {
    store: S,
    clock: C,
    next_seq: u64,
    prev_hash: [u8; HASH_BYTES],
    last_ts: u64,
    redaction_policy: RedactionPolicy,
}

impl<S, C> LedgerAppender<S, C>
where
    S: LedgerCfStore,
    C: Clock,
{
    /// Opens an appender and recovers its tip from existing ledger rows.
    pub fn open(store: S, clock: C) -> Result<Self> {
        Self::open_with_policy(store, clock, RedactionPolicy::default())
    }

    /// Opens an appender with an explicit redaction policy.
    pub fn open_with_policy(store: S, clock: C, redaction_policy: RedactionPolicy) -> Result<Self> {
        let (next_seq, prev_hash, last_ts) = recover_tip(&store)?;
        Ok(Self {
            store,
            clock,
            next_seq,
            prev_hash,
            last_ts,
            redaction_policy,
        })
    }

    /// Appends one chained entry and returns its provenance reference.
    pub fn append(
        &mut self,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        let prepared = self.prepare(kind, subject, payload, actor)?;
        self.commit_prepared(&prepared)
    }

    /// Builds the next ledger row without mutating the store or appender tip.
    pub fn prepare(
        &self,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<PreparedLedgerEntry> {
        self.prepare_at(
            PreparePosition {
                seq: self.next_seq,
                prev_hash: self.prev_hash,
                last_ts: self.last_ts,
            },
            kind,
            subject,
            payload,
            actor,
        )
    }

    /// Builds the row that must follow an uncommitted staged ledger row.
    pub fn prepare_after(
        &self,
        predecessor: &PreparedLedgerEntry,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<PreparedLedgerEntry> {
        let seq = predecessor
            .seq()
            .checked_add(1)
            .ok_or_else(|| CalyxError::ledger_chain_broken("ledger sequence exhausted"))?;
        self.prepare_at(
            PreparePosition {
                seq,
                prev_hash: predecessor.entry_hash(),
                last_ts: predecessor.ts(),
            },
            kind,
            subject,
            payload,
            actor,
        )
    }

    fn prepare_at(
        &self,
        position: PreparePosition,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<PreparedLedgerEntry> {
        self.redaction_policy.check_payload_with_policy(&payload)?;
        self.verify_tip()?;
        actor.validate()?;
        let actor = self.redaction_policy.apply_to_actor(actor);
        actor.validate()?;
        let ts = self.next_ts_after(position.last_ts)?;
        let entry = LedgerEntry::new(
            position.seq,
            position.prev_hash,
            kind,
            subject,
            payload,
            actor,
            ts,
        );
        let bytes = encode(&entry);
        Ok(PreparedLedgerEntry { entry, bytes })
    }

    /// Commits a previously prepared row and advances the recovered tip.
    pub fn commit_prepared(&mut self, prepared: &PreparedLedgerEntry) -> Result<LedgerRef> {
        if prepared.entry.seq != self.next_seq || prepared.entry.prev_hash != self.prev_hash {
            return Err(CalyxError::ledger_chain_broken(format!(
                "prepared ledger seq {} does not match appender next_seq {}",
                prepared.entry.seq, self.next_seq
            )));
        }
        self.store.put_new(prepared.entry.seq, prepared.bytes())?;
        let anchor = LedgerHeadAnchor::new(
            prepared.entry.seq.saturating_add(1),
            prepared.entry.entry_hash,
        )?;
        self.store.put_head_anchor(&anchor)?;
        self.last_ts = prepared.entry.ts;
        self.next_seq = prepared
            .entry
            .seq
            .checked_add(1)
            .ok_or_else(|| CalyxError::ledger_chain_broken("ledger sequence exhausted"))?;
        self.prev_hash = prepared.entry.entry_hash;
        Ok(prepared.ledger_ref())
    }

    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub const fn prev_hash(&self) -> [u8; HASH_BYTES] {
        self.prev_hash
    }

    pub const fn last_ts(&self) -> u64 {
        self.last_ts
    }

    pub fn refresh_tip_from_store(&mut self) -> Result<()> {
        let (next_seq, prev_hash, last_ts) = recover_tip(&self.store)?;
        if next_seq < self.next_seq || (next_seq == self.next_seq && prev_hash != self.prev_hash) {
            return Err(CalyxError::ledger_chain_broken(format!(
                "ledger tip diverged: appender expected next_seq {}, store has {}",
                self.next_seq, next_seq
            )));
        }
        self.next_seq = next_seq;
        self.prev_hash = prev_hash;
        self.last_ts = last_ts;
        Ok(())
    }

    pub fn scan_entries(&self) -> Result<Vec<LedgerEntry>> {
        self.store
            .scan()?
            .into_iter()
            .map(|row| decode(&row.bytes))
            .collect()
    }

    pub fn scan_recent_entries_by_kind(
        &self,
        kind: EntryKind,
        n: usize,
    ) -> Result<Vec<LedgerEntry>> {
        self.store
            .scan_recent_by_kind(kind, n)?
            .into_iter()
            .map(|row| decode(&row.bytes))
            .collect()
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn into_store(self) -> S {
        self.store
    }

    fn verify_tip(&self) -> Result<()> {
        let (next_seq, prev_hash, last_ts) = recover_tip(&self.store)?;
        if next_seq == self.next_seq && prev_hash == self.prev_hash && last_ts == self.last_ts {
            return Ok(());
        }
        Err(CalyxError::ledger_chain_broken(format!(
            "ledger tip changed: appender expected next_seq {}, store has {}",
            self.next_seq, next_seq
        )))
    }

    fn next_ts_after(&self, last_ts: u64) -> Result<u64> {
        let clock_ts = self.clock.now();
        Ok(if clock_ts <= last_ts {
            last_ts
                .checked_add(1)
                .ok_or_else(|| CalyxError::ledger_chain_broken("ledger timestamp exhausted"))?
        } else {
            clock_ts
        })
    }
}

pub fn reject_delete(seq: u64) -> Result<()> {
    Err(append_only_violation(format!(
        "delete forbidden for ledger seq {seq}"
    )))
}

pub fn reject_tombstone(seq: u64) -> Result<()> {
    Err(append_only_violation(format!(
        "tombstone forbidden for ledger seq {seq}"
    )))
}

/// Recovers the appender tip from a store, honoring its external head anchor.
///
/// This is the single tip-recovery routine for every ledger write path. When
/// the store exposes a [`LedgerHeadAnchor`] it takes the anchored fast path and
/// fails closed on an end-truncated ledger; otherwise it validates the full
/// hash chain by scan. Any additional write path (e.g. reproduce appends) must
/// use this rather than re-deriving the tip, so the anchor stays authoritative.
pub(crate) fn recover_tip(store: &impl LedgerCfStore) -> Result<(u64, [u8; HASH_BYTES], u64)> {
    if let Some(anchor) = store.head_anchor()? {
        if anchor.height == 0 {
            return Ok((0, [0_u8; HASH_BYTES], 0));
        }
        let last_seq = anchor.height.saturating_sub(1);
        let row = store.read_seq(last_seq)?.ok_or_else(|| {
            CalyxError::ledger_chain_broken(format!(
                "ledger end-truncated: anchored head {} requires missing seq {last_seq}",
                anchor.height
            ))
        })?;
        if row.seq != last_seq {
            return Err(CalyxError::ledger_corrupt(format!(
                "ledger read_seq({last_seq}) returned row seq {}",
                row.seq
            )));
        }
        let entry = decode(&row.bytes)?;
        if entry.seq != row.seq {
            return Err(CalyxError::ledger_corrupt(format!(
                "ledger key seq {} != encoded seq {}",
                row.seq, entry.seq
            )));
        }
        if entry.entry_hash != anchor.tip_hash {
            return Err(CalyxError::ledger_chain_broken(
                "ledger anchored tip hash does not match last row",
            ));
        }
        return Ok((anchor.height, anchor.tip_hash, entry.ts));
    }
    let mut next_seq = 0_u64;
    let mut prev_hash = [0_u8; HASH_BYTES];
    let mut last_ts = 0_u64;
    for row in store.scan()? {
        if row.seq != next_seq {
            return Err(CalyxError::ledger_chain_broken(format!(
                "ledger seq gap: expected {}, found {}",
                next_seq, row.seq
            )));
        }
        let entry = decode(&row.bytes)?;
        if entry.seq != row.seq {
            return Err(CalyxError::ledger_corrupt(format!(
                "ledger key seq {} != encoded seq {}",
                row.seq, entry.seq
            )));
        }
        if entry.prev_hash != prev_hash {
            return Err(CalyxError::ledger_chain_broken(format!(
                "ledger seq {} prev_hash does not match prior entry",
                row.seq
            )));
        }
        prev_hash = entry.entry_hash;
        last_ts = entry.ts;
        next_seq = next_seq
            .checked_add(1)
            .ok_or_else(|| CalyxError::ledger_chain_broken("ledger sequence exhausted"))?;
    }
    let anchor = store.head_anchor()?;
    verify_recovered_tip(anchor.as_ref(), next_seq, prev_hash)?;
    Ok((next_seq, prev_hash, last_ts))
}

fn append_only_violation(message: impl Into<String>) -> CalyxError {
    CalyxError::ledger_append_only_violation(message)
}
