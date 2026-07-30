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
    /// Changed keys contributed by each indexed slot CF.
    ///
    /// A slot id belongs to exactly one panel *name* (#1776), which is why these
    /// scans need no `Base`-style attribution — but **not** to one panel
    /// *version*: a new generation of the same panel reuses its slot ids, so two
    /// live generations of one panel would count each other here. Latent while a
    /// superseded generation is inert; tracked in #1905, which is also where the
    /// stronger observation lives (every slot write also writes its Base row, so
    /// these scans may be redundant once that is established for every writer).
    pub slot_keys: BTreeMap<SlotId, usize>,
    /// Distinct constellations to reconcile: the union of the scoped `Base`
    /// keys and every slot CF's keys.
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
            "panel_version={} base_seq={} pinned_seq={} distinct_changed={} base_scanned={} base_panel={} base_other_panels={} base_unattributed={}{}{slots}",
            self.panel_version,
            self.base_seq,
            self.pinned_seq,
            self.changed.len(),
            self.base_keys_scanned,
            self.base_keys_panel,
            self.base_keys_other_panels,
            self.base_keys_unattributed,
            if self.slot_keys.is_empty() { "" } else { " " },
        )
    }
}

/// Measures one panel generation's changed-key delta against a pinned snapshot.
///
/// The `Base` scan is scoped to `panel_version`; the slot scans rely on slot ids
/// being globally unique per panel *name* (see `slot_keys` for the residual this
/// leaves, tracked in #1905). This is the single definition of "how far behind is
/// this generation" in the workspace: both the query path's bounded
/// reconciliation check and the maintainer's refresh trigger call it.
///
/// # Errors
///
/// Fails closed when the MVCC changed-key history cannot prove the requested
/// range, or when a changed key is not a well-formed `CxId`.
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
    for slot in query_slots {
        let cf = ColumnFamily::slot(slot);
        let keys = vault.changed_cf_keys_after_snapshot(snapshot, cf, base_seq)?;
        slot_keys.insert(slot, keys.len());
        for key in keys {
            changed.insert(cx_id_from_key(&key, cf)?);
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
