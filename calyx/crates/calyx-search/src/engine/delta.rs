use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Constellation, CxId, SlotId, SlotVector};
use calyx_sextant::IndexSearchHit;

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;
use crate::persisted::{PersistedPanelMembership, PersistedSearchIndexes};

use super::support::SearchReadSnapshot;

/// Bounded changed-key budget for reconciling a lagging generation against the
/// current snapshot. Beyond this the query fails closed with
/// `CALYX_SEARCH_DELTA_REBASE_REQUIRED` rather than doing unbounded work, so
/// operational surfaces need to be able to report the limit (issue #1891).
pub const MAX_RECONCILED_DELTA_KEYS: usize = 8_192;
const DELTA_REBASE_CODE: &str = "CALYX_SEARCH_DELTA_REBASE_REQUIRED";

/// A reconciled query masked rows out of its own index and hydrated nothing to
/// replace them (#1907). Distinct from a genuine miss on purpose: #1896 traded
/// "empty result" for "stale index" for the *matching* case, and this is the
/// case that trade left unguarded.
pub const RECONCILED_REPLACEMENTS_MISSING_CODE: &str =
    "CALYX_SEARCH_RECONCILED_REPLACEMENTS_MISSING";

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
    /// Changed slot keys with no matching key in the panel-scoped `Base` set.
    ///
    /// A single reported number, not three classified ones (#1935). Each such
    /// key is one of: another generation's row (slot ids are reused across
    /// generations of the same panel), an orphan awaiting GC, or a live row of
    /// this generation whose slot CF was compacted more recently than `Base`.
    /// Telling them apart costs two point reads and a `Base` decode **per key**
    /// — ~1M of them on the agent-transcript panel, which blew the
    /// measurement's 30 s reader lease — and none of the three changes any
    /// decision, because slot keys do not contribute to `changed` at all
    /// (#1905).
    ///
    /// The size of this number relative to `slot_keys` is still the diagnosis:
    /// close to the whole lane means that lane was compacted after `Base`.
    pub slot_keys_not_in_base_set: usize,
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
            "panel_version={} base_seq={} pinned_seq={} distinct_changed={} base_scanned={} base_panel={} base_other_panels={} base_unattributed={} slot_keys_not_in_base_set={}{}{slots}",
            self.panel_version,
            self.base_seq,
            self.pinned_seq,
            self.changed.len(),
            self.base_keys_scanned,
            self.base_keys_panel,
            self.base_keys_other_panels,
            self.base_keys_unattributed,
            self.slot_keys_not_in_base_set,
            if self.slot_keys.is_empty() { "" } else { " " },
        )
    }
}

/// Current-snapshot membership derived from one hash-verified immutable
/// sidecar plus its bounded, panel-scoped Base delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciledPanelMembership {
    pub panel_version: u32,
    pub base_seq: u64,
    pub covered_to_seq: u64,
    pub manifest_sha256: String,
    pub sidecar_sha256: String,
    pub sidecar_rows: usize,
    pub changed_keys: usize,
    pub ids: Vec<CxId>,
}

fn require_reconcilable_delta(composition: &PanelDeltaComposition, surface: &str) -> CliResult<()> {
    if composition.changed_len() <= MAX_RECONCILED_DELTA_KEYS {
        return Ok(());
    }
    Err(CalyxError {
        code: DELTA_REBASE_CODE,
        message: format!(
            "{surface} delta contains {} changed keys between manifest base seq {} and pinned seq {}, exceeding the bounded reconciliation limit {MAX_RECONCILED_DELTA_KEYS} ({})",
            composition.changed_len(),
            composition.base_seq,
            composition.pinned_seq,
            composition.composition()
        ),
        remediation: "rebuild the exact panel search generation, then retry; the immutable generation is too far behind for bounded current-snapshot reconciliation",
    }
    .into())
}

fn merge_membership_delta(
    persisted: PersistedPanelMembership,
    changed: &[(CxId, bool)],
    covered_to_seq: u64,
) -> CliResult<ReconciledPanelMembership> {
    let sidecar_rows = persisted.ids.len();
    let capacity = sidecar_rows.checked_add(changed.len()).ok_or_else(|| CalyxError {
        code: "CALYX_SEARCH_MEMBERSHIP_CAPACITY_OVERFLOW",
        message: format!(
            "panel {} membership capacity overflow: sidecar_rows={sidecar_rows} changed_keys={}",
            persisted.panel_version,
            changed.len()
        ),
        remediation: "preserve the manifest and changed-key history; inspect the reported counts before retrying",
    })?;
    let mut ids = Vec::new();
    ids.try_reserve_exact(capacity).map_err(|error| CalyxError {
        code: "CALYX_SEARCH_MEMBERSHIP_RESERVE_FAILED",
        message: format!(
            "reserve panel {} reconciled membership capacity {capacity}: {error}",
            persisted.panel_version
        ),
        remediation: "release completed corpus owners and retry; if allocation still fails, inspect process-private memory and the reported membership counts",
    })?;

    let mut persisted_index = 0_usize;
    let mut changed_index = 0_usize;
    while persisted_index < persisted.ids.len() || changed_index < changed.len() {
        match (
            persisted.ids.get(persisted_index).copied(),
            changed.get(changed_index).copied(),
        ) {
            (Some(existing), Some((changed_id, _))) if existing < changed_id => {
                ids.push(existing);
                persisted_index += 1;
            }
            (Some(existing), Some((changed_id, live_in_panel))) if existing == changed_id => {
                if live_in_panel {
                    ids.push(changed_id);
                }
                persisted_index += 1;
                changed_index += 1;
            }
            (Some(_), Some((changed_id, live_in_panel))) => {
                if live_in_panel {
                    ids.push(changed_id);
                }
                changed_index += 1;
            }
            (Some(existing), None) => {
                ids.push(existing);
                persisted_index += 1;
            }
            (None, Some((changed_id, live_in_panel))) => {
                if live_in_panel {
                    ids.push(changed_id);
                }
                changed_index += 1;
            }
            (None, None) => break,
        }
    }

    Ok(ReconciledPanelMembership {
        panel_version: persisted.panel_version,
        base_seq: persisted.base_seq,
        covered_to_seq,
        manifest_sha256: persisted.manifest_sha256,
        sidecar_sha256: persisted.sidecar_sha256,
        sidecar_rows,
        changed_keys: changed.len(),
        ids,
    })
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
/// - **compaction/tiering** rewrites SSTs. This does **not** leave the
///   commit-domain rows alone, and believing it did is what produced #1935: the
///   SST row format carries no per-row commit sequence, so a compaction output
///   is named at the maximum sequence of its inputs and recovery restores every
///   row of that file *at the file's sequence*. Repeated compaction therefore
///   ratchets a whole column family's rows forward to recent sequences.
///   Measured on the production vault: `slot_35` held all 68,691 rows in 16
///   SSTs whose oldest name was seq 202247, so 100.00% of the CF reported as
///   changed, while the less-recently-compacted `base` reported 1.14%.
///   Compaction preserves values, so these are re-stamps, not writes, and the
///   cross-check below proves that per key against the row's own `Base` slot
///   hash rather than inferring corruption from the sequence alone.
///
/// This is not only an audit of today's call sites. `mvcc::store` refuses, at
/// commit, any batch carrying a live quantized-slot write with neither a
/// same-batch nor a visible `Base` row (`"live quantized slot write has no
/// visible or same-batch Base row … refusing unscoped search-index mutation"`).
///
/// The scans still run, but only to **report** each slot lane's changed-key
/// count. They deliberately do not classify each key, and there is no read-side
/// cross-check (#1935).
///
/// There used to be one: a changed slot key absent from the panel's `Base` set
/// was classified per key, and a key belonging to a live row of *this*
/// generation was raised as `CALYX_ASTER_CORRUPT_SHARD` — remediation `restore
/// from restic/snapshot`. Two things were wrong with it.
///
/// It could not be right. A key is in this scan because it has a *version*
/// after the bound, and compaction gives a whole column family new versions at
/// once, so "slot changed, `Base` did not" is the expected state after a
/// compaction and carries no information about any writer. On the production
/// vault it fired against a provably healthy shard, naming an irreversible
/// remedy for undamaged data.
///
/// It also could not be afforded. Classifying each key costs two point reads
/// plus a `Base` decode; on the agent-transcript panel that is ~1M keys across
/// 15 slot lanes, and the measurement blew its 30 s reader lease. The old code
/// only appeared cheap because it aborted on the first key it misjudged.
///
/// The invariant it was trying to police — every live quantized slot write is
/// staged atomically with its own `Base` row — is now **enforced at commit** by
/// `mvcc::store::search_panels_affected_by_batch`, which is the only place that
/// can tell a write from a rewrite. Enforced there, the panel-scoped `Base` set
/// is complete by construction and needs no read-side confirmation.
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
    // Counted, never classified. `keys.len()` is free once the scan has run;
    // deciding what each key *is* costs two point reads and a `Base` decode per
    // key, produced only diagnostics, and could not be answered correctly from
    // this evidence anyway (#1935).
    let mut slot_keys_not_in_base_set = 0_usize;
    for slot in query_slots {
        let cf = ColumnFamily::slot(slot);
        let keys = vault.changed_cf_keys_after_snapshot(snapshot, cf, base_seq)?;
        slot_keys.insert(slot, keys.len());
        for key in keys {
            if !changed.contains(&cx_id_from_key(&key, cf)?) {
                slot_keys_not_in_base_set += 1;
            }
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
        slot_keys_not_in_base_set,
        changed,
    })
}

/// Reconciles one immutable panel-membership sidecar to an exact pinned
/// snapshot using the same bounded changed-key law as search queries.
///
/// The sidecar remains the authoritative compact baseline. Only Base keys that
/// changed after its generation are point-read: a live row for this panel is
/// inserted/replaced, while a tombstoned or moved row is removed. Exceeding the
/// query reconciliation bound fails closed with the same rebase error as a
/// search query; there is no global Base scan.
pub fn reconcile_panel_membership<C: Clock>(
    vault: &AsterVault<C>,
    indexes: &PersistedSearchIndexes,
    snapshot: calyx_aster::mvcc::Snapshot,
) -> CliResult<ReconciledPanelMembership> {
    let persisted = indexes.panel_membership()?;
    if persisted.base_seq > snapshot.seq() {
        return Err(CalyxError {
            code: "CALYX_SEARCH_MEMBERSHIP_FUTURE_GENERATION",
            message: format!(
                "panel {} membership generation seq {} is newer than pinned snapshot {}",
                persisted.panel_version,
                persisted.base_seq,
                snapshot.seq()
            ),
            remediation: "re-pin after the published generation, or repair a manifest whose Base sequence is ahead of the vault",
        }
        .into());
    }
    if snapshot.derived_content_seq() <= persisted.base_seq {
        let covered_to_seq = snapshot.seq();
        return merge_membership_delta(persisted, &[], covered_to_seq);
    }

    let composition = measure_panel_delta(
        vault,
        snapshot,
        persisted.panel_version,
        persisted.base_seq,
        std::iter::empty(),
    )?;
    require_reconcilable_delta(&composition, "panel membership")?;
    let mut changed = Vec::new();
    changed
        .try_reserve_exact(composition.changed_len())
        .map_err(|error| CalyxError {
            code: "CALYX_SEARCH_MEMBERSHIP_DELTA_RESERVE_FAILED",
            message: format!(
                "reserve {} changed membership identities for panel {}: {error}",
                composition.changed_len(),
                persisted.panel_version
            ),
            remediation: "release completed corpus owners and retry; if allocation still fails, inspect process-private memory and the reported delta count",
        })?;
    for cx_id in &composition.changed {
        let present = vault
            .read_cf_snapshot(snapshot, ColumnFamily::Base, cx_id.as_bytes())?
            .is_some();
        let live_in_panel = if present {
            vault.get_base_at_snapshot(*cx_id, snapshot)?.panel_version == persisted.panel_version
        } else {
            false
        };
        changed.push((*cx_id, live_in_panel));
    }
    merge_membership_delta(persisted, &changed, snapshot.seq())
}

pub(super) struct SearchDelta {
    changed: BTreeSet<CxId>,
    docs: BTreeMap<CxId, Constellation>,
    vectors: BTreeMap<SlotId, BTreeMap<CxId, SlotVector>>,
    /// Per query slot, how many live changed rows **declare** that slot.
    ///
    /// Together with [`Self::absent`] this is the expectation `vectors` is
    /// judged against (#1907), as a conservation law rather than a heuristic:
    /// every declared slot on a live row hydrates to exactly one vector, and
    /// that vector is either usable (counted in `vectors`) or explicitly
    /// [`SlotVector::Absent`] (counted in `absent`). So
    ///
    /// ```text
    ///     declared[slot] == vectors[slot].len() + absent[slot]
    /// ```
    ///
    /// must hold for every query slot. Any shortfall is a row the delta masked
    /// out of the index and then failed to account for.
    declared: BTreeMap<SlotId, usize>,
    /// Per query slot, how many declared vectors hydrated to `Absent`.
    ///
    /// `Absent` is an explicit absence, not a zero vector: an inactive lens, a
    /// modality that does not apply, or an unavailable lens all produce it
    /// legitimately, and such a row genuinely contributes no recall. Counting
    /// it separately is what lets the conservation law above stay exact instead
    /// of firing on a healthy parked lane.
    absent: BTreeMap<SlotId, usize>,
    covered_to_seq: Option<u64>,
}

impl SearchDelta {
    pub(super) fn empty() -> Self {
        Self {
            changed: BTreeSet::new(),
            docs: BTreeMap::new(),
            vectors: BTreeMap::new(),
            declared: BTreeMap::new(),
            absent: BTreeMap::new(),
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
        require_reconcilable_delta(&composition, "search")?;
        let mut docs = BTreeMap::new();
        let mut vectors = query_slots
            .iter()
            .map(|slot| (*slot, BTreeMap::new()))
            .collect::<BTreeMap<_, _>>();
        let mut declared = query_slots
            .iter()
            .map(|slot| (*slot, 0_usize))
            .collect::<BTreeMap<_, _>>();
        let mut absent = query_slots
            .iter()
            .map(|slot| (*slot, 0_usize))
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
            // Slot membership must come from the row's declared slot set, NOT
            // from `base.slots` (#1907): `get_base_at_snapshot` clears that map
            // by design, so filtering through it selected nothing for every row
            // ever — the delta masked the whole index and replaced none of it.
            let row_slots = vault.declared_slot_ids_at_snapshot(*cx_id, read.snapshot())?;
            let available_query_slots = query_slots
                .iter()
                .filter(|slot| row_slots.contains(slot))
                .copied()
                .collect::<BTreeSet<_>>();
            for slot in &available_query_slots {
                *declared.get_mut(slot).expect("query slot initialized") += 1;
            }
            let cx = vault.get_selected_slots_at_snapshot(
                *cx_id,
                read.snapshot(),
                available_query_slots.iter().copied(),
            )?;
            for slot in query_slots {
                match cx.slots.get(slot) {
                    Some(SlotVector::Absent { .. }) => {
                        *absent.get_mut(slot).expect("query slot initialized") += 1;
                    }
                    Some(vector) => {
                        vectors
                            .get_mut(slot)
                            .expect("query slot initialized")
                            .insert(*cx_id, vector.clone());
                    }
                    None => {}
                }
            }
            docs.insert(*cx_id, cx);
        }
        trace.emit_detail(
            "delta.scan.done",
            None,
            Some(changed_keys.len()),
            Some(format!(
                "live_replacements={} tombstoned_or_moved={} covered_to_seq={} declared={} measured={}",
                docs.len(),
                changed_keys.len().saturating_sub(docs.len()),
                read.seq(),
                summarize_per_slot(&declared),
                summarize_per_slot(
                    &vectors
                        .iter()
                        .map(|(slot, rows)| (*slot, rows.len()))
                        .collect()
                ),
            )),
        );
        Ok(Self {
            changed: changed_keys,
            docs,
            vectors,
            declared,
            absent,
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

    fn declared_for(&self, slot: SlotId) -> usize {
        self.declared.get(&slot).copied().unwrap_or_default()
    }

    fn absent_for(&self, slot: SlotId) -> usize {
        self.absent.get(&slot).copied().unwrap_or_default()
    }
}

/// `slot_<id>=<count>` for every query slot, so a delta detail line names the
/// per-slot numbers rather than a total that hides which lane is short.
fn summarize_per_slot(counts: &BTreeMap<SlotId, usize>) -> String {
    counts
        .iter()
        .map(|(slot, count)| format!("slot_{}={count}", slot.get()))
        .collect::<Vec<_>>()
        .join(",")
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
        let declared = delta.declared_for(*slot);
        let absent = delta.absent_for(*slot);
        let replacements = delta.vectors_for(*slot).len();
        trace.emit_detail(
            "search_slot.delta.start",
            Some(*slot),
            Some(k),
            Some(format!(
                "changed={} declared={declared} replacements={replacements} absent={absent}",
                delta.changed.len(),
            )),
        );
        // Conservation check — the guard #1907 needed and did not have.
        //
        // `changed` masks its rows out of the persisted index unconditionally,
        // whether or not a replacement exists. So a row that is masked but not
        // accounted for is not a ranking nuance: that part of the corpus is
        // *deleted* from the answer, and the caller is handed a short result (or
        // `Ok(vec![])`) that reads as "nothing matched". #1907 was exactly that
        // — `base.slots` is cleared by `get_base_at_snapshot`, so the slot
        // filter selected nothing for every row ever, every replacement set was
        // empty, and whole-index recall silently went to zero after a backfill.
        //
        // Every declared slot on a live row hydrates to exactly one vector, and
        // that vector is either usable or explicitly `Absent`. So the identity
        // below is exact, not a threshold, and it holds for the healthy states
        // that a cruder "replacements is empty" test would false-close on:
        //
        // - purely tombstoned changed rows never reach `declared` at all;
        // - a changed row that does not carry this slot is not declared;
        // - a parked/inapplicable lens declares its slot and hydrates `Absent`,
        //   which is counted, so an all-absent lane reconciles quietly.
        if declared != replacements.saturating_add(absent) {
            return Err(CalyxError {
                code: RECONCILED_REPLACEMENTS_MISSING_CODE,
                message: format!(
                    "reconciled search for slot {slot} masked {} changed constellation(s) out of the persisted index, but could not \
                     account for what it masked: {declared} live row(s) of this panel declare the slot, yet only {replacements} \
                     hydrated a usable vector and {absent} an explicit Absent ({} unaccounted). Every declared slot on a live row \
                     hydrates to exactly one vector, so the shortfall means those rows were removed from the index and nothing took \
                     their place — the result would silently omit them rather than rank them",
                    delta.changed.len(),
                    declared.saturating_sub(replacements.saturating_add(absent)),
                ),
                remediation: "rebuild the exact panel search generation to serve recall from a current index, then repair the delta \
                              hydration path; do not read this as an empty result — the index holds rows this query was entitled to see",
            }
            .into());
        }
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
