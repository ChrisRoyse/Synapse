use crate::cf::{ColumnFamily, anchor_key, base_key, slot_key};
use crate::mvcc::{CfRead, Snapshot};
use calyx_core::{Anchor, CalyxError, Clock, Constellation, CxId, Result, Seq, SlotId, VaultStore};
use std::collections::{BTreeMap, BTreeSet};

use super::{AsterVault, anchor_merge, encode, ledger_hook, prepared};

const COMPRESSED_SLOT_TAG: u8 = 16;

/// Authoritative disposition of one Aster put, decided while holding the
/// durable commit lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutDisposition {
    Inserted,
    ExistingIdentical,
    ExistingAnchorsMerged { added: usize },
    InBatchDuplicate { anchors_added: usize },
}

/// Ordered result for one submitted constellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PutOutcome {
    pub cx_id: CxId,
    pub disposition: PutDisposition,
}

#[derive(Clone, Copy)]
pub(super) enum DuplicatePutPolicy {
    StrictConstellation,
    ContentObservation,
}

impl PutOutcome {
    pub const fn inserted(self) -> bool {
        matches!(self.disposition, PutDisposition::Inserted)
    }

    pub const fn deduped(self) -> bool {
        !self.inserted()
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Writes one constellation and returns the locked, authoritative
    /// insert/dedup/anchor-merge decision without rereading Base afterward.
    pub fn put_with_outcome(&self, constellation: Constellation) -> Result<PutOutcome> {
        self.put_with_outcome_policy(constellation, DuplicatePutPolicy::StrictConstellation)
    }

    /// Inserts a content-addressed observation or reports its locked duplicate
    /// outcome. On duplicates, the first-write metadata/scalars/input pointer
    /// remain authoritative while compatible anchors are merged. Content hash,
    /// panel, modality, redaction, and slot differences still fail closed.
    pub fn put_observation_with_outcome(&self, constellation: Constellation) -> Result<PutOutcome> {
        self.put_with_outcome_policy(constellation, DuplicatePutPolicy::ContentObservation)
    }

    fn put_with_outcome_policy(
        &self,
        constellation: Constellation,
        duplicate_policy: DuplicatePutPolicy,
    ) -> Result<PutOutcome> {
        if constellation.vault_id != self.vault_id {
            return Err(CalyxError::vault_access_denied(
                "constellation belongs to another vault",
            ));
        }
        constellation.validate_schema()?;
        let prepared = prepared::PreparedConstellationEncoding::new(&constellation)?;

        self.with_durable_commit_lock(move || {
            let mut constellation = constellation;
            let id = constellation.cx_id;
            let base_key = base_key(id);
            let latest = self.snapshot();
            let snapshot = self.snapshot_handle(latest);
            if let Some(existing) = self.rows.read_at(
                snapshot.snapshot(),
                ColumnFamily::Base,
                &base_key,
                &self.clock,
            )? {
                let base_bytes = prepared.encode_base(&constellation)?;
                if existing == base_bytes {
                    return Ok(PutOutcome {
                        cx_id: id,
                        disposition: PutDisposition::ExistingIdentical,
                    });
                }
                let mut merged = self.get_at_snapshot(id, snapshot.snapshot())?;
                let added = match duplicate_policy {
                    DuplicatePutPolicy::StrictConstellation => {
                        anchor_merge::merge_duplicate_anchors(&mut merged, &constellation)?
                    }
                    DuplicatePutPolicy::ContentObservation => {
                        anchor_merge::merge_observation_anchors(&mut merged, &constellation)?
                    }
                };
                if !added.is_empty() {
                    let rows = anchor_merge::stage_anchor_merge_rows(id, &merged, &added)?;
                    self.commit_rows_locked(&rows)?;
                }
                return Ok(PutOutcome {
                    cx_id: id,
                    disposition: if added.is_empty() {
                        PutDisposition::ExistingIdentical
                    } else {
                        PutDisposition::ExistingAnchorsMerged { added: added.len() }
                    },
                });
            }

            let mut rows = Vec::new();
            let mut hook_guard = match &self.ledger_hook {
                Some(hook) => Some(ledger_hook::lock_hook(hook)?),
                None => None,
            };
            let staged_ledger = if let Some(hook) = hook_guard.as_deref() {
                let staged = ledger_hook::stage_ingest(hook, &mut rows, &constellation)?;
                constellation.provenance = staged
                    .first()
                    .ok_or_else(|| CalyxError::ledger_group_commit_failed("no staged ledger rows"))?
                    .ledger_ref();
                Some(staged)
            } else {
                constellation.provenance = self.stage_raw_ingest_ledger_locked(
                    &mut rows,
                    constellation.cx_id,
                    ledger_hook::ingest_payload(&constellation),
                )?;
                None
            };
            prepared::stage_validated_constellation_rows(&mut rows, &constellation, prepared)?;
            self.commit_rows_locked(&rows)?;
            if let (Some(hook), Some(staged)) = (hook_guard.take(), staged_ledger.as_ref()) {
                self.commit_persistent_ledger_staged_locked(hook, staged, "put_constellation")?;
            }
            Ok(PutOutcome {
                cx_id: id,
                disposition: PutDisposition::Inserted,
            })
        })
    }

    /// Reads one stored constellation through an already-pinned snapshot lease.
    pub fn get_at_snapshot(&self, id: CxId, snapshot: Snapshot) -> Result<Constellation> {
        let constellation = self.read_base_at_snapshot(id, snapshot)?;
        let slot_ids: Vec<SlotId> = constellation.slots.keys().copied().collect();
        self.hydrate_slots_at_snapshot(id, snapshot, constellation, slot_ids)
    }

    /// Reads one stored constellation through an already-pinned snapshot lease,
    /// hydrating only the requested slots. The requested slots must be present
    /// in the Base CF row, otherwise the derived caller state is stale.
    pub fn get_selected_slots_at_snapshot<I>(
        &self,
        id: CxId,
        snapshot: Snapshot,
        selected_slots: I,
    ) -> Result<Constellation>
    where
        I: IntoIterator<Item = SlotId>,
    {
        let constellation = self.read_base_at_snapshot(id, snapshot)?;
        let available_slots = constellation.slots.keys().copied().collect::<BTreeSet<_>>();
        let slot_ids = selected_slots.into_iter().collect::<BTreeSet<_>>();
        for slot in &slot_ids {
            if !available_slots.contains(slot) {
                return Err(CalyxError::stale_derived(format!(
                    "selected slot {slot} is absent from Base row for {id}"
                )));
            }
        }
        self.hydrate_slots_at_snapshot(id, snapshot, constellation, slot_ids)
    }

    /// Merges proposed duplicate anchors into an existing constellation with the
    /// same semantics as duplicate ingest: dedup by kind, reject conflicts, and
    /// commit at most one base+anchor row batch.
    pub fn merge_anchors<I>(&self, id: CxId, anchors: I) -> Result<usize>
    where
        I: IntoIterator<Item = Anchor>,
    {
        let anchors = anchors.into_iter().collect::<Vec<_>>();
        for anchor in &anchors {
            anchor.validate_schema()?;
        }
        if anchors.is_empty() {
            return Ok(0);
        }
        self.with_durable_commit_lock(|| {
            let latest = self.snapshot();
            let mut merged = self.get(id, latest)?;
            let mut incoming = merged.clone();
            incoming.anchors.extend(anchors.iter().cloned());
            incoming.flags.ungrounded = incoming.anchors.is_empty();
            let added = anchor_merge::merge_duplicate_anchors(&mut merged, &incoming)?;
            if !added.is_empty() {
                let rows = anchor_merge::stage_anchor_merge_rows(id, &merged, &added)?;
                self.commit_rows_locked(&rows)?;
            }
            Ok(added.len())
        })
    }

    fn hydrate_slots_at_snapshot<I>(
        &self,
        id: CxId,
        snapshot: Snapshot,
        mut constellation: Constellation,
        slot_ids: I,
    ) -> Result<Constellation>
    where
        I: IntoIterator<Item = SlotId>,
    {
        let slot_ids: Vec<SlotId> = slot_ids.into_iter().collect();
        if slot_ids.is_empty() {
            constellation.slots.clear();
            return Ok(constellation);
        }
        let reads: Vec<_> = slot_ids
            .iter()
            .map(|slot| CfRead::new(ColumnFamily::slot(*slot), slot_key(id)))
            .collect();
        let values = self.rows.read_batch(snapshot, &reads, &self.clock)?;
        let mut slots = BTreeMap::new();
        for (slot, value) in slot_ids.into_iter().zip(values) {
            let value =
                value.ok_or_else(|| CalyxError::aster_corrupt_shard("slot CF row missing"))?;
            let vector = match encode::decode_slot_vector(&value) {
                Ok(vector) => vector,
                Err(error) if value.first().copied() == Some(COMPRESSED_SLOT_TAG) => {
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "AsterVault::get_at_snapshot encountered compressed slot CF row for slot {slot}; use a compression-aware read path instead of raw sidecar fallback ({error})"
                    )));
                }
                Err(error) => return Err(error),
            };
            slots.insert(slot, vector);
        }
        constellation.slots = slots;
        Ok(constellation)
    }

    /// Reads the Base CF row only, preserving metadata, anchors, and stored
    /// provenance without hydrating slot vectors.
    pub fn get_base_at_snapshot(&self, id: CxId, snapshot: Snapshot) -> Result<Constellation> {
        let mut constellation = self.read_base_at_snapshot(id, snapshot)?;
        constellation.slots.clear();
        Ok(constellation)
    }

    fn read_base_at_snapshot(&self, id: CxId, snapshot: Snapshot) -> Result<Constellation> {
        let base = self
            .rows
            .read_at(snapshot, ColumnFamily::Base, &base_key(id), &self.clock)?
            .ok_or_else(|| CalyxError::stale_derived("constellation missing at snapshot"))?;
        encode::decode_constellation_base(&base)
    }
}

impl<C> VaultStore for AsterVault<C>
where
    C: Clock,
{
    fn put(&self, constellation: Constellation) -> Result<CxId> {
        self.put_with_outcome(constellation)
            .map(|outcome| outcome.cx_id)
    }

    fn get(&self, id: CxId, snapshot: Seq) -> Result<Constellation> {
        let snapshot = self.snapshot_handle(snapshot);
        self.get_at_snapshot(id, snapshot.snapshot())
    }

    fn anchor(&self, id: CxId, anchor: Anchor) -> Result<()> {
        anchor.validate_schema()?;
        self.with_recurrence_write_lock(|| {
            let latest = self.snapshot();
            let mut constellation = self.get(id, latest)?;
            constellation.anchors.push(anchor.clone());
            constellation.flags.ungrounded = constellation.anchors.is_empty();
            let rows = [
                (
                    ColumnFamily::Base,
                    base_key(id),
                    encode::encode_constellation_base(&constellation)?,
                ),
                (
                    ColumnFamily::Anchors,
                    anchor_key(id, &anchor.kind),
                    encode::encode_anchor(&anchor)?,
                ),
            ];
            let rows = rows
                .into_iter()
                .map(|(cf, key, value)| encode::WriteRow { cf, key, value })
                .collect::<Vec<_>>();
            self.commit_rows_locked(&rows)?;
            Ok(())
        })
    }

    fn snapshot(&self) -> Seq {
        self.latest_seq()
    }
}
