use std::collections::BTreeSet;

use super::{AsterVault, encode, ledger_hook};
use crate::cf::{ColumnFamily, anchor_key, base_key, ledger_key};
use crate::ledger_view::parse_aster_ledger_seq;
use calyx_core::{Anchor, CalyxError, Clock, CxId, LedgerRef, Result, SystemClock, VaultStore};
use calyx_ledger::{
    ActorId, EntryKind, LedgerAppender, LedgerCfStore, LedgerHeadAnchor, LedgerRow, SubjectId,
    declare_batch_members, decode as decode_ledger, require_base_stamp_declared,
};

struct AnchorBatchLedgerInput {
    kind: EntryKind,
    subject: SubjectId,
    payload: Vec<u8>,
    actor: ActorId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiCxAnchorBatchOutcome {
    pub ledger_ref: Option<LedgerRef>,
    pub requested_anchor_count: usize,
    pub existing_anchor_count: usize,
    pub written_anchor_count: usize,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Adds multiple anchors and stamps the stored base row with one shared ledger ref.
    ///
    /// This is for one semantic grounding event that has more than one anchor axis. The batch is
    /// idempotent only when every requested anchor already exists and the Base provenance points to
    /// the same requested ledger entry. A partial pre-existing batch fails closed so a legacy
    /// unstamped anchor is never silently upgraded.
    pub fn anchors_with_ledger_entry(
        &self,
        id: CxId,
        anchors: Vec<Anchor>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<LedgerRef> {
        validate_anchor_batch(&anchors)?;
        // This path rewrites the Base row's `provenance`, so the shape must be
        // one the search-side coverage table can serve (#2095). Checked before
        // the commit lock is taken: a refusal here costs nothing and names the
        // writer that chose the shape.
        //
        // Unlike the multi-constellation sibling this path does NOT add a
        // `batch_members` declaration (#2096), for two reasons. It covers
        // exactly one constellation, which its `Cx` subject already names, so a
        // member list would add nothing a reader can use. And the idempotent
        // branch below compares the requested payload byte-for-byte against the
        // stored entry (`validate_existing_batch_ledger`): rewriting the payload
        // here would make every anchor batch already committed on a live vault
        // fail that comparison as `CALYX_ASTER_CORRUPT_SHARD` on its next
        // idempotent replay.
        require_base_stamp_declared("anchors_with_ledger_entry", kind, &subject)?;
        let entry = AnchorBatchLedgerInput {
            kind,
            subject,
            payload,
            actor,
        };
        self.with_durable_commit_lock(|| {
            let latest = self.snapshot();
            let mut constellation = self.get(id, latest)?;
            let mut missing = Vec::new();
            let mut existing_count = 0usize;
            let mut superseding_count = 0usize;
            for anchor in &anchors {
                match classify_anchor_state(self, latest, id, &constellation, anchor)? {
                    AnchorState::Existing => existing_count += 1,
                    // A superseding anchor is written exactly like a missing one
                    // — the Anchors CF key is `(id, kind)`, so the row is
                    // overwritten in place, and the Base row's same-kind anchor
                    // is replaced rather than duplicated by
                    // `anchor_batch_rows_with_ledger_ref`.
                    AnchorState::Missing => missing.push(anchor.clone()),
                    AnchorState::Superseding => {
                        superseding_count += 1;
                        missing.push(anchor.clone());
                    }
                }
            }
            if existing_count == anchors.len() {
                validate_existing_batch_ledger(
                    self,
                    latest,
                    id,
                    &constellation.provenance,
                    &entry,
                )?;
                return Ok(constellation.provenance.clone());
            }
            // A batch that mixes already-present anchors with genuinely MISSING
            // ones still fails closed: that is the shape in which a legacy
            // unstamped anchor gets silently upgraded, which is what this refusal
            // exists to prevent. A supersession is not a missing anchor — the row
            // is present and is being rewritten under the declared
            // last-observation-wins rule (#2072) — so `existing + superseding`
            // is a legitimate batch and is admitted.
            if existing_count != 0 && existing_count + superseding_count != anchors.len() {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "partial anchor batch for {id}: {existing_count} existing anchors, \
                     {superseding_count} superseding anchors and {} missing anchors",
                    missing.len().saturating_sub(superseding_count)
                )));
            }

            let Some(hook) = &self.ledger_hook else {
                let store = AnchorBatchRawLedgerStore { vault: self };
                let appender = LedgerAppender::open(store, SystemClock)?;
                let prepared =
                    appender.prepare(entry.kind, entry.subject, entry.payload, entry.actor)?;
                let ledger_ref = prepared.ledger_ref();
                let mut rows = anchor_batch_rows_with_ledger_ref(
                    id,
                    &mut constellation,
                    &missing,
                    &ledger_ref,
                )?;
                rows.push(encode::WriteRow {
                    cf: ColumnFamily::Ledger,
                    key: ledger_key(prepared.seq()),
                    value: prepared.bytes().to_vec(),
                });
                self.commit_rows_locked(&rows)?;
                return Ok(ledger_ref);
            };

            let guard = ledger_hook::lock_hook(hook)?;
            let staged = guard.stage_with_checkpoints(
                entry.kind,
                entry.subject,
                entry.payload,
                entry.actor,
            )?;
            let ledger_ref = staged
                .first()
                .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                .ledger_ref();
            let mut rows =
                anchor_batch_rows_with_ledger_ref(id, &mut constellation, &missing, &ledger_ref)?;
            rows.extend(staged.iter().map(|row| encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key: row.key().to_vec(),
                value: row.value().to_vec(),
            }));
            self.commit_rows_locked(&rows)?;
            self.commit_persistent_ledger_staged_locked(
                guard,
                &staged,
                "anchors_with_ledger_entry",
            )?;
            Ok(ledger_ref)
        })
    }

    /// Adds anchors for many constellations in one durable commit.
    ///
    /// Each requested `(CxId, AnchorKind)` is checked at the same snapshot. Exact
    /// pre-existing anchors are accepted as idempotent; missing anchors are
    /// staged with one shared ledger entry and become visible atomically in one
    /// MVCC/WAL commit. Conflicting anchor rows or Base/Anchors divergence fail
    /// before any new anchor is written.
    pub fn anchors_for_many_with_ledger_entry(
        &self,
        entries: Vec<(CxId, Vec<Anchor>)>,
        kind: EntryKind,
        subject: SubjectId,
        payload: Vec<u8>,
        actor: ActorId,
    ) -> Result<MultiCxAnchorBatchOutcome> {
        let requested_anchor_count = validate_multi_cx_anchor_batch(&entries)?;
        require_base_stamp_declared("anchors_for_many_with_ledger_entry", kind, &subject)?;
        self.with_durable_commit_lock(move || {
            let latest = self.snapshot();
            let mut existing_anchor_count = 0usize;
            let mut missing_entries = Vec::<(CxId, calyx_core::Constellation, Vec<Anchor>)>::new();
            for (id, anchors) in &entries {
                let constellation = self.get(*id, latest)?;
                let mut missing = Vec::new();
                for anchor in anchors {
                    match classify_anchor_state(self, latest, *id, &constellation, anchor)? {
                        AnchorState::Existing => {
                            existing_anchor_count = existing_anchor_count.saturating_add(1);
                        }
                        AnchorState::Missing | AnchorState::Superseding => {
                            missing.push(anchor.clone());
                        }
                    }
                }
                if !missing.is_empty() {
                    missing_entries.push((*id, constellation, missing));
                }
            }
            if missing_entries.is_empty() {
                return Ok(MultiCxAnchorBatchOutcome {
                    ledger_ref: None,
                    requested_anchor_count,
                    existing_anchor_count,
                    written_anchor_count: 0,
                });
            }

            // #2096: this entry is about to become the provenance of every Base
            // row below, under a subject that names none of them. Declare the
            // membership in the payload so "which constellations does seq N
            // cover" is answerable from the ledger alone.
            //
            // The members are the constellations actually RESTAMPED — the
            // `missing_entries` set, not the full request. A constellation whose
            // anchors were all already present keeps its earlier provenance and
            // is genuinely not covered by this entry; listing it would make the
            // declaration a lie in the one direction that matters, since the
            // reader treats a complete declaration as authoritative about
            // non-membership too.
            let members = missing_entries
                .iter()
                .map(|(id, _constellation, _anchors)| *id)
                .collect::<Vec<_>>();
            let payload = declare_batch_members(&payload, &members)?;
            let entry = AnchorBatchLedgerInput {
                kind,
                subject,
                payload,
                actor,
            };

            let Some(hook) = &self.ledger_hook else {
                let store = AnchorBatchRawLedgerStore { vault: self };
                let appender = LedgerAppender::open(store, SystemClock)?;
                let prepared =
                    appender.prepare(entry.kind, entry.subject, entry.payload, entry.actor)?;
                let ledger_ref = prepared.ledger_ref();
                let mut rows = multi_cx_anchor_rows_with_ledger_ref(missing_entries, &ledger_ref)?;
                rows.push(encode::WriteRow {
                    cf: ColumnFamily::Ledger,
                    key: ledger_key(prepared.seq()),
                    value: prepared.bytes().to_vec(),
                });
                self.commit_rows_locked(&rows)?;
                let written_anchor_count =
                    requested_anchor_count.saturating_sub(existing_anchor_count);
                return Ok(MultiCxAnchorBatchOutcome {
                    ledger_ref: Some(ledger_ref),
                    requested_anchor_count,
                    existing_anchor_count,
                    written_anchor_count,
                });
            };

            let guard = ledger_hook::lock_hook(hook)?;
            let staged = guard.stage_with_checkpoints(
                entry.kind,
                entry.subject,
                entry.payload,
                entry.actor,
            )?;
            let ledger_ref = staged
                .first()
                .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                .ledger_ref();
            let mut rows = multi_cx_anchor_rows_with_ledger_ref(missing_entries, &ledger_ref)?;
            rows.extend(staged.iter().map(|row| encode::WriteRow {
                cf: ColumnFamily::Ledger,
                key: row.key().to_vec(),
                value: row.value().to_vec(),
            }));
            self.commit_rows_locked(&rows)?;
            self.commit_persistent_ledger_staged_locked(
                guard,
                &staged,
                "anchors_for_many_with_ledger_entry",
            )?;
            let written_anchor_count = requested_anchor_count.saturating_sub(existing_anchor_count);
            Ok(MultiCxAnchorBatchOutcome {
                ledger_ref: Some(ledger_ref),
                requested_anchor_count,
                existing_anchor_count,
                written_anchor_count,
            })
        })
    }
}

enum AnchorState {
    Existing,
    Missing,
    /// The persisted anchor for this `(CxId, kind)` holds a different value and
    /// its observation is not newer than the requested one, so the request
    /// supersedes it under the declared write rule (#2072).
    Superseding,
}

fn classify_anchor_state<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: u64,
    id: CxId,
    constellation: &calyx_core::Constellation,
    anchor: &Anchor,
) -> Result<AnchorState> {
    let key = anchor_key(id, &anchor.kind);
    let anchor_bytes = encode::encode_anchor(anchor)?;
    let matching_base_anchor_count = constellation
        .anchors
        .iter()
        .filter(|existing| existing.kind == anchor.kind)
        .count();
    let exact_base_anchor_count = constellation
        .anchors
        .iter()
        .filter(|existing| *existing == anchor)
        .count();
    match vault.read_cf_at(snapshot, ColumnFamily::Anchors, &key)? {
        Some(existing_bytes) => {
            let stored_anchor = encode::decode_anchor(&existing_bytes)?;
            if existing_bytes != anchor_bytes || stored_anchor != *anchor {
                // #2072. Two writers proposing different values for one
                // `(CxId, kind)` is a WRITE CONFLICT, not a damaged shard, and
                // it has a declared answer.
                //
                // ## The rule: last observation wins, ties to the incoming write
                //
                // An anchor is an *observation* of a real outcome, and
                // `observed_at` exists on it precisely so two observations of
                // the same subject can be ordered. The newer observation of a
                // mutable subject is the better one, so it supersedes.
                //
                // Ties go to the incoming write, and that is a choice rather
                // than an accident. The population that produced this on the
                // live daemon is agent-state resurrection re-deriving
                // `synapse:agent_end_state` from the same terminal event — same
                // `observed_at`, different derived value — so an equality
                // refusal is not a tiebreak but a permanent stall: the write is
                // deterministic, so retrying is guaranteed to fail identically,
                // and refusing leaves the row carrying the value the *less*
                // informed pass computed while its caller records
                // `operation_committed=true` with the anchor failed. A replay
                // re-derives from the whole journal; the original wrote
                // incrementally. Between two observations of one instant, the
                // later writer is at least as authoritative.
                //
                // This is never silent: the supersession is committed inside the
                // batch's ledger entry, so it has durable provenance, and both
                // values are named in the warn below.
                //
                // A STRICTLY OLDER observation is refused, because
                // last-observation-wins run backwards would let a late replay of
                // stale state clobber a newer reading. That refusal now carries
                // its own code and a remediation that reconciles rather than one
                // that tells an operator to restore a healthy vault from backup.
                if anchor.observed_at < stored_anchor.observed_at {
                    return Err(CalyxError::aster_anchor_value_conflict(format!(
                        "conflicting Anchors CF row for {id} {:?}: the persisted anchor holds a \
                         STRICTLY NEWER observation than the requested one \
                         (stored observed_at={} source={:?}, requested observed_at={} source={:?}), \
                         so last-observation-wins refuses the write; the row remains grounded by \
                         the newer observation and NOTHING is corrupt",
                        anchor.kind,
                        stored_anchor.observed_at,
                        stored_anchor.source,
                        anchor.observed_at,
                        anchor.source,
                    )));
                }
                tracing::warn!(
                    code = "CALYX_ASTER_ANCHOR_VALUE_SUPERSEDED",
                    cx_id = %id,
                    anchor_kind = ?anchor.kind,
                    stored_observed_at = stored_anchor.observed_at,
                    stored_source = %stored_anchor.source,
                    stored_value = ?stored_anchor.value,
                    requested_observed_at = anchor.observed_at,
                    requested_source = %anchor.source,
                    requested_value = ?anchor.value,
                    "an anchor rewrite proposed a different value for an existing (CxId, kind); \
                     last observation wins, so the requested value supersedes the stored one \
                     inside this batch's ledger entry"
                );
                return Ok(AnchorState::Superseding);
            }
            if matching_base_anchor_count != 1 || exact_base_anchor_count != 1 {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "Anchors CF row for {id} {:?} matches request but Base row has {} \
                     matching-kind anchors",
                    anchor.kind, matching_base_anchor_count
                )));
            }
            Ok(AnchorState::Existing)
        }
        None => {
            if matching_base_anchor_count != 0 {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "Base row for {id} already has {} {:?} anchors but Anchors CF row is missing",
                    matching_base_anchor_count, anchor.kind
                )));
            }
            Ok(AnchorState::Missing)
        }
    }
}

fn validate_existing_batch_ledger<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: u64,
    id: CxId,
    ledger_ref: &LedgerRef,
    entry: &AnchorBatchLedgerInput,
) -> Result<()> {
    let row = vault
        .read_cf_at(snapshot, ColumnFamily::Ledger, &ledger_key(ledger_ref.seq))?
        .ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "matching anchor batch for {id} has provenance seq {} but Ledger CF row is missing",
                ledger_ref.seq
            ))
        })?;
    let stored = decode_ledger(&row)?;
    if stored.seq != ledger_ref.seq || stored.entry_hash != ledger_ref.hash {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "matching anchor batch for {id} has provenance ref that does not match Ledger CF row"
        )));
    }
    if stored.kind != entry.kind
        || stored.subject != entry.subject
        || stored.payload != entry.payload
        || stored.actor != entry.actor
    {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "matching anchor batch for {id} is not backed by the requested ledger entry"
        )));
    }
    Ok(())
}

fn anchor_batch_rows_with_ledger_ref(
    id: CxId,
    constellation: &mut calyx_core::Constellation,
    anchors: &[Anchor],
    ledger_ref: &LedgerRef,
) -> Result<Vec<encode::WriteRow>> {
    constellation.provenance = ledger_ref.clone();
    // Drop any same-kind anchor first, so a supersession replaces the stored
    // observation instead of leaving the Base row carrying two anchors on one
    // axis (#2072). This is a no-op for a genuinely missing anchor:
    // `classify_anchor_state` has already refused the case where the Anchors CF
    // row is absent while the Base row holds an anchor of that kind.
    constellation
        .anchors
        .retain(|stored| !anchors.iter().any(|anchor| anchor.kind == stored.kind));
    constellation.anchors.extend_from_slice(anchors);
    constellation.flags.ungrounded = false;
    constellation.validate_schema()?;
    let mut rows = Vec::with_capacity(1 + anchors.len());
    rows.push(encode::WriteRow {
        cf: ColumnFamily::Base,
        key: base_key(id),
        value: encode::encode_constellation_base(constellation)?,
    });
    for anchor in anchors {
        rows.push(encode::WriteRow {
            cf: ColumnFamily::Anchors,
            key: anchor_key(id, &anchor.kind),
            value: encode::encode_anchor(anchor)?,
        });
    }
    Ok(rows)
}

fn multi_cx_anchor_rows_with_ledger_ref(
    entries: Vec<(CxId, calyx_core::Constellation, Vec<Anchor>)>,
    ledger_ref: &LedgerRef,
) -> Result<Vec<encode::WriteRow>> {
    let anchor_count = entries
        .iter()
        .map(|(_id, _constellation, anchors)| anchors.len())
        .sum::<usize>();
    let mut rows = Vec::with_capacity(entries.len().saturating_add(anchor_count));
    for (id, mut constellation, anchors) in entries {
        rows.extend(anchor_batch_rows_with_ledger_ref(
            id,
            &mut constellation,
            &anchors,
            ledger_ref,
        )?);
    }
    Ok(rows)
}

fn validate_anchor_batch(anchors: &[Anchor]) -> Result<()> {
    if anchors.is_empty() {
        return Err(CalyxError::aster_corrupt_shard(
            "ledger-stamped anchor batch must contain at least one anchor",
        ));
    }
    let mut kinds = BTreeSet::new();
    for anchor in anchors {
        anchor.validate_schema()?;
        if !kinds.insert(anchor.kind.clone()) {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "ledger-stamped anchor batch contains duplicate anchor kind {:?}",
                anchor.kind
            )));
        }
    }
    Ok(())
}

fn validate_multi_cx_anchor_batch(entries: &[(CxId, Vec<Anchor>)]) -> Result<usize> {
    if entries.is_empty() {
        return Err(CalyxError::aster_corrupt_shard(
            "multi-constellation ledger-stamped anchor batch must contain at least one constellation",
        ));
    }
    let mut requested = 0usize;
    let mut unique = BTreeSet::new();
    for (id, anchors) in entries {
        validate_anchor_batch(anchors)?;
        for anchor in anchors {
            requested = requested.saturating_add(1);
            if !unique.insert((*id, anchor.kind.clone())) {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "multi-constellation ledger-stamped anchor batch contains duplicate request for {id} {:?}",
                    anchor.kind
                )));
            }
        }
    }
    Ok(requested)
}

struct AnchorBatchRawLedgerStore<'a, C> {
    vault: &'a AsterVault<C>,
}

impl<C> LedgerCfStore for AnchorBatchRawLedgerStore<'_, C>
where
    C: Clock,
{
    fn scan(&self) -> Result<Vec<LedgerRow>> {
        let mut rows = Vec::new();
        for (key, bytes) in self
            .vault
            .scan_cf_at(self.vault.snapshot(), ColumnFamily::Ledger)?
        {
            rows.push(LedgerRow {
                seq: parse_aster_ledger_seq(&key)?,
                bytes,
            });
        }
        rows.sort_by_key(|row| row.seq);
        Ok(rows)
    }

    fn put_new(&mut self, seq: u64, bytes: &[u8]) -> Result<()> {
        self.vault
            .write_raw_ledger_row_without_hook(seq, bytes)
            .map(|_| ())
    }

    fn head_anchor(&self) -> Result<Option<LedgerHeadAnchor>> {
        let Some(durable) = &self.vault.durable else {
            return Ok(None);
        };
        let anchor = crate::ledger_head::read_head_anchor(durable.root())?;
        if anchor.is_none() {
            let rows = self.scan()?;
            return crate::ledger_head::require_head_anchor_for_rows(durable.root(), anchor, &rows);
        }
        Ok(anchor)
    }

    fn put_head_anchor(&mut self, anchor: &LedgerHeadAnchor) -> Result<()> {
        self.vault.validate_committed_ledger_head_anchor(anchor)
    }
}
