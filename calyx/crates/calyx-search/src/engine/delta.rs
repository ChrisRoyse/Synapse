use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Constellation, CxId, SlotId, SlotVector};
use calyx_sextant::IndexSearchHit;

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;
use crate::persisted::PersistedSearchIndexes;

use super::support::SearchReadSnapshot;

const MAX_RECONCILED_DELTA_KEYS: usize = 8_192;
const DELTA_REBASE_CODE: &str = "CALYX_SEARCH_DELTA_REBASE_REQUIRED";

pub(super) struct SearchDelta {
    changed: BTreeSet<CxId>,
    docs: BTreeMap<CxId, Constellation>,
    vectors: BTreeMap<SlotId, BTreeMap<CxId, SlotVector>>,
    covered_to_seq: Option<u64>,
}

impl SearchDelta {
    pub(super) fn empty() -> Self {
        Self {
            changed: BTreeSet::new(),
            docs: BTreeMap::new(),
            vectors: BTreeMap::new(),
            covered_to_seq: None,
        }
    }

    pub(super) fn collect<C: Clock>(
        vault: &AsterVault<C>,
        indexes: &PersistedSearchIndexes,
        read: &SearchReadSnapshot<'_, C>,
        query_slots: &BTreeSet<SlotId>,
        trace: &mut SearchTracer<'_>,
    ) -> CliResult<Self> {
        let base_seq = indexes.base_seq();
        if read.derived_content_seq() <= base_seq {
            return Ok(Self::empty());
        }
        trace.emit_detail(
            "delta.scan.start",
            None,
            Some(query_slots.len()),
            Some(format!(
                "manifest_base_seq={base_seq} pinned_seq={} panel_content_seq={}",
                read.seq(),
                read.derived_content_seq()
            )),
        );
        let mut changed_keys = BTreeSet::new();
        for key in
            vault.changed_cf_keys_after_snapshot(read.snapshot(), ColumnFamily::Base, base_seq)?
        {
            changed_keys.insert(cx_id_from_key(&key, ColumnFamily::Base)?);
        }
        for slot in query_slots {
            let cf = ColumnFamily::slot(*slot);
            for key in vault.changed_cf_keys_after_snapshot(read.snapshot(), cf, base_seq)? {
                changed_keys.insert(cx_id_from_key(&key, cf)?);
            }
        }
        if changed_keys.len() > MAX_RECONCILED_DELTA_KEYS {
            return Err(CalyxError {
                code: DELTA_REBASE_CODE,
                message: format!(
                    "search delta contains {} changed keys between manifest base seq {base_seq} and pinned seq {}, exceeding the bounded reconciliation limit {MAX_RECONCILED_DELTA_KEYS}",
                    changed_keys.len(),
                    read.seq()
                ),
                remediation: "rebuild the exact panel search generation, then retry; the immutable generation is too far behind for bounded current-snapshot reconciliation",
            }
            .into());
        }
        let mut docs = BTreeMap::new();
        let mut vectors = query_slots
            .iter()
            .map(|slot| (*slot, BTreeMap::new()))
            .collect::<BTreeMap<_, _>>();
        for cx_id in &changed_keys {
            let base =
                vault.read_cf_snapshot(read.snapshot(), ColumnFamily::Base, cx_id.as_bytes())?;
            if base.is_none() {
                continue;
            }
            let base = vault.get_base_at_snapshot(*cx_id, read.snapshot())?;
            if base.panel_version != indexes.panel_version() {
                continue;
            }
            let available_query_slots = query_slots
                .iter()
                .filter(|slot| base.slots.contains_key(slot))
                .copied()
                .collect::<BTreeSet<_>>();
            let cx = vault.get_selected_slots_at_snapshot(
                *cx_id,
                read.snapshot(),
                available_query_slots.iter().copied(),
            )?;
            for slot in query_slots {
                if let Some(vector) = cx
                    .slots
                    .get(slot)
                    .filter(|vector| !matches!(vector, SlotVector::Absent { .. }))
                {
                    vectors
                        .get_mut(slot)
                        .expect("query slot initialized")
                        .insert(*cx_id, vector.clone());
                }
            }
            docs.insert(*cx_id, cx);
        }
        trace.emit_detail(
            "delta.scan.done",
            None,
            Some(changed_keys.len()),
            Some(format!(
                "live_replacements={} tombstoned_or_moved={} covered_to_seq={}",
                docs.len(),
                changed_keys.len().saturating_sub(docs.len()),
                read.seq()
            )),
        );
        Ok(Self {
            changed: changed_keys,
            docs,
            vectors,
            covered_to_seq: Some(read.seq()),
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    pub(super) fn changed(&self) -> &BTreeSet<CxId> {
        &self.changed
    }

    pub(super) fn docs(&self) -> &BTreeMap<CxId, Constellation> {
        &self.docs
    }

    pub(super) fn covered_to_seq(&self) -> Option<u64> {
        self.covered_to_seq
    }

    fn vectors_for(&self, slot: SlotId) -> &BTreeMap<CxId, SlotVector> {
        self.vectors.get(&slot).unwrap_or_else(|| empty_vectors())
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search_slots_reconciled(
    indexes: &PersistedSearchIndexes,
    query_vectors: &[(SlotId, SlotVector)],
    k: usize,
    candidates: Option<&BTreeSet<CxId>>,
    delta: &SearchDelta,
    trace: &mut SearchTracer<'_>,
) -> CliResult<BTreeMap<SlotId, Vec<IndexSearchHit>>> {
    let mut out = BTreeMap::new();
    for (slot, query) in query_vectors {
        trace.emit_detail(
            "search_slot.delta.start",
            Some(*slot),
            Some(k),
            Some(format!(
                "changed={} replacements={}",
                delta.changed.len(),
                delta.vectors_for(*slot).len()
            )),
        );
        let hits = indexes.search_reconciled(
            *slot,
            query,
            k,
            candidates,
            &delta.changed,
            delta.vectors_for(*slot),
        )?;
        trace.emit("search_slot.delta.done", Some(*slot), Some(hits.len()));
        if !hits.is_empty() {
            out.insert(*slot, hits);
        }
    }
    Ok(out)
}

fn cx_id_from_key(key: &[u8], cf: ColumnFamily) -> CliResult<CxId> {
    let bytes: [u8; 16] = key.try_into().map_err(|_| {
        CalyxError::aster_corrupt_shard(format!(
            "changed-key journal for {} contains a non-CxId key with {} bytes",
            cf.name(),
            key.len()
        ))
    })?;
    Ok(CxId::from_bytes(bytes))
}

fn empty_vectors() -> &'static BTreeMap<CxId, SlotVector> {
    static EMPTY: std::sync::OnceLock<BTreeMap<CxId, SlotVector>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(BTreeMap::new)
}
