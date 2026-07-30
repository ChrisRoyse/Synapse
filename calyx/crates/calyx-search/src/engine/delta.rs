use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Constellation, CxId, SlotId, SlotVector};
use calyx_sextant::IndexSearchHit;

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;
use crate::persisted::PersistedSearchIndexes;

use super::support::SearchReadSnapshot;

/// Bounded changed-key budget for reconciling a lagging generation against the
/// current snapshot. Beyond this the query fails closed with
/// `CALYX_SEARCH_DELTA_REBASE_REQUIRED` rather than doing unbounded work, so
/// operational surfaces need to be able to report the limit (issue #1891).
pub const MAX_RECONCILED_DELTA_KEYS: usize = 8_192;
const DELTA_REBASE_CODE: &str = "CALYX_SEARCH_DELTA_REBASE_REQUIRED";

/// The exact panel-scoped changed-key delta a query is judged against, with the
/// composition that produced the count (#1901).
///
/// One measurement, one definition: both the query path's bounded
/// reconciliation check and the unattended maintainer's refresh trigger read
/// this, so the number that decides a rebuild and the number that fails a query
/// closed cannot drift apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelDeltaComposition {
    /// Panel version the delta was scoped to.
    pub panel_version: u32,
    /// Generation base sequence the delta starts after (exclusive).
    pub base_seq: u64,
    /// Pinned snapshot sequence the delta ends at (inclusive).
    pub pinned_seq: u64,
    /// Changed `Base` keys across every panel, before scoping.
    pub base_keys_scanned: usize,
    /// Changed `Base` keys attributed to this panel.
    pub base_keys_panel: usize,
    /// Changed `Base` keys attributed to some other panel and excluded.
    pub base_keys_other_panels: usize,
    /// Changed `Base` keys whose visible history is entirely tombstoned.
    pub base_keys_unattributed: usize,
    /// Changed keys observed in each indexed slot CF.
    ///
    /// A slot id belongs to exactly one panel *name* (#1776) — but **not** to one
    /// panel *version*: a new generation of the same panel reuses its slot ids,
    /// so `cf/slot_03` holds rows from every generation of that panel. These
    /// counts are therefore **reported, not counted** (#1905): they no longer
    /// contribute to `changed`, so an inflated slot contribution stays visible
    /// here without charging this generation's budget for another generation's
    /// ingest.
    pub slot_keys: BTreeMap<SlotId, usize>,
    /// Changed slot keys belonging to a different generation, excluded.
    pub slot_keys_other_generation: usize,
    /// Changed slot keys with no visible `Base` row, excluded.
    ///
    /// Orphan slot rows awaiting GC. They were never reconcilable — the
    /// reconciliation skips a key whose `Base` row is absent — so counting them
    /// only ever inflated the budget.
    pub slot_keys_orphaned: usize,
    /// Distinct constellations to reconcile: exactly the panel-scoped `Base`
    /// changed keys.
    ///
    /// Slot keys are deliberately **not** unioned in. Every live slot write is
    /// staged in the same atomic batch as its own `Base` row — see
    /// `measure_panel_delta` for the enumeration and the commit-time guard that
    /// holds it — so a slot key that this panel generation must reconcile is
    /// already here, and one that is not here belongs to another generation or
    /// to no live row at all.
    pub changed: BTreeSet<CxId>,
}

impl PanelDeltaComposition {
    /// Distinct constellations this delta must reconcile.
    #[must_use]
    pub fn changed_len(&self) -> usize {
        self.changed.len()
    }

    /// One-line composition, so an error names where its count came from rather
    /// than only how large it is.
    #[must_use]
    pub fn composition(&self) -> String {
        let slots = self
            .slot_keys
            .iter()
            .map(|(slot, count)| format!("slot_{}={count}", slot.get()))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "panel_version={} base_seq={} pinned_seq={} distinct_changed={} base_scanned={} base_panel={} base_other_panels={} base_unattributed={} slot_other_generation={} slot_orphaned={}{}{slots}",
            self.panel_version,
            self.base_seq,
            self.pinned_seq,
            self.changed.len(),
            self.base_keys_scanned,
            self.base_keys_panel,
            self.base_keys_other_panels,
            self.base_keys_unattributed,
            self.slot_keys_other_generation,
            self.slot_keys_orphaned,
            if self.slot_keys.is_empty() { "" } else { " " },
        )
    }
}

/// Measures one panel generation's changed-key delta against a pinned snapshot.
///
/// This is the single definition of "how far behind is this generation" in the
/// workspace: both the query path's bounded reconciliation check and the
/// maintainer's refresh trigger call it.
///
/// # Why the delta is the panel-scoped `Base` set, and nothing else (#1905)
///
/// #1901 scoped the `Base` half. The slot half was left unscoped on the grounds
/// that a slot id belongs to exactly one panel (#1776) — true of a panel *name*
/// and false of a panel *version*. A new generation reuses its predecessor's
/// slot ids, so `cf/slot_03` holds rows from every generation of that panel and
/// an unscoped scan charges one generation for another's ingest.
///
/// Rather than attribute each slot key, the scans stop contributing at all,
/// because the panel-scoped `Base` set is already complete. Every writer that
/// can stage a row into a quantized slot CF:
///
/// - **ingest** (`prepared::stage_validated_constellation_rows`, single and
///   batch) stages `Base` and every slot row in one atomic batch;
/// - **`put_slot_vector`** — the only qualified slot write — stages the slot row
///   *and* the rewritten `Base` row in one atomic batch, because the `Base` slot
///   hash **is** the vector's integrity record (#1888);
/// - **erase** (`erase::targets`) reaches `collect_slot_targets` only alongside
///   pushing the row's own `Base` key, so `Base` is tombstoned with it — and a
///   fully-tombstoned chain is returned in the panel's keys as `unattributed`,
///   precisely so a deleted row is still masked;
/// - **orphan-slot GC** deletes slot rows that have *no visible `Base` row*,
///   which the reconciliation skips anyway (`base.is_none()` → `continue`);
/// - **compaction/tiering** moves SSTs, not commit-domain rows.
///
/// This is not only an audit of today's call sites. `mvcc::store` refuses, at
/// commit, any batch carrying a live quantized-slot write with neither a
/// same-batch nor a visible `Base` row (`"live quantized slot write has no
/// visible or same-batch Base row … refusing unscoped search-index mutation"`).
///
/// The scans still run, as a **cross-check rather than a contribution**: a
/// changed slot key that is absent from the panel's `Base` set must be
/// explicable, and each one is classified as another generation's or as an
/// orphan. A key that is neither — a live row of *this* generation whose slot
/// changed without its `Base` — would mean a writer has broken the invariant
/// this scoping rests on, and that fails closed rather than silently
/// under-reconciling.
///
/// # Errors
///
/// Fails closed when the MVCC changed-key history cannot prove the requested
/// range, when a changed key is not a well-formed `CxId`, or when a changed slot
/// key belongs to this generation but its `Base` row did not change with it.
pub fn measure_panel_delta<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: calyx_aster::mvcc::Snapshot,
    panel_version: u32,
    base_seq: u64,
    query_slots: impl IntoIterator<Item = SlotId>,
) -> CliResult<PanelDeltaComposition> {
    let scoped =
        vault.changed_base_keys_after_snapshot_for_panel(snapshot, base_seq, panel_version)?;
    let mut changed = BTreeSet::new();
    for key in &scoped.keys {
        changed.insert(cx_id_from_key(key, ColumnFamily::Base)?);
    }
    let mut slot_keys = BTreeMap::new();
    let mut slot_keys_other_generation = 0_usize;
    let mut slot_keys_orphaned = 0_usize;
    for slot in query_slots {
        let cf = ColumnFamily::slot(slot);
        let keys = vault.changed_cf_keys_after_snapshot(snapshot, cf, base_seq)?;
        slot_keys.insert(slot, keys.len());
        for key in keys {
            let cx_id = cx_id_from_key(&key, cf)?;
            if changed.contains(&cx_id) {
                continue;
            }
            if vault
                .read_cf_snapshot(snapshot, ColumnFamily::Base, cx_id.as_bytes())?
                .is_none()
            {
                slot_keys_orphaned += 1;
                continue;
            }
            let base = vault.get_base_at_snapshot(cx_id, snapshot)?;
            if base.panel_version == panel_version {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "constellation {cx_id} changed in {} after seq {base_seq} but its Base row did not, while both belong to panel {panel_version}; \
                     every live slot write is staged in the same atomic batch as its own Base row, so this means a writer bypassed that contract and the \
                     panel-scoped reconciliation delta is no longer complete. Repair the writer rather than widening the delta",
                    cf.name()
                ))
                .into());
            }
            slot_keys_other_generation += 1;
        }
    }
    Ok(PanelDeltaComposition {
        panel_version,
        base_seq,
        pinned_seq: snapshot.seq(),
        base_keys_scanned: scoped.scanned,
        base_keys_panel: scoped.panel,
        base_keys_other_panels: scoped.other_panels,
        base_keys_unattributed: scoped.unattributed,
        slot_keys,
        slot_keys_other_generation,
        slot_keys_orphaned,
        changed,
    })
}

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
        let composition = measure_panel_delta(
            vault,
            read.snapshot(),
            indexes.panel_version(),
            base_seq,
            query_slots.iter().copied(),
        )?;
        let changed_keys = composition.changed.clone();
        trace.emit_detail(
            "delta.scan.scoped",
            None,
            Some(changed_keys.len()),
            Some(composition.composition()),
        );
        if changed_keys.len() > MAX_RECONCILED_DELTA_KEYS {
            return Err(CalyxError {
                code: DELTA_REBASE_CODE,
                message: format!(
                    "search delta contains {} changed keys between manifest base seq {base_seq} and pinned seq {}, exceeding the bounded reconciliation limit {MAX_RECONCILED_DELTA_KEYS} ({})",
                    changed_keys.len(),
                    read.seq(),
                    composition.composition()
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
