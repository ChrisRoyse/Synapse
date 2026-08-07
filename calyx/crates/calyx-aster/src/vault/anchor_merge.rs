use std::collections::BTreeSet;

use super::encode::{self, WriteRow};
use crate::cf::{ColumnFamily, anchor_key, base_key};
use crate::dedup::{AnchorConflictResult, check_anchor_conflict};
use crate::recurrence::FREQUENCY_SCALAR;
use calyx_core::{Anchor, CalyxError, Constellation, CxId, LedgerRef, Result, SlotId};

/// Names exactly how a re-measured constellation's slot **membership** differs
/// from the stored row's, or `None` when the two declare the same slot ids.
///
/// This is separated from the generic identity diff because "slots" on its own
/// is not actionable: the caller needs to know *which* slot it added before it
/// can allocate that slot an id and a new panel generation (#1903).
///
/// # Membership, not bytes (#2070)
///
/// This used to fail a re-measure whose slot ids matched exactly but whose
/// per-slot content hashes differed, and that classification was wrong. The
/// frozen contract of a panel generation is its slot **layout** — which lanes
/// exist and what each lane's id means — because that is what a query plan, a
/// slot CF and a lens provenance record are written against. The *bytes* in a
/// lane are a measurement, and a measurement of the same row taken again is an
/// update, not a contract violation.
///
/// The cost of conflating the two was total: on the deployed daemon
/// `syn-timeline-v1` re-measured one stored `Base` row and produced different
/// content hashes for slots 3 and 103 — its two title-derived lexical lanes —
/// with `slots_added=[]`, `slots_removed=[]` and both slot lists identical. The
/// coverage sweep therefore failed on page 1 of `CF_TIMELINE` on every tick and
/// its 174,993-row backlog never moved a single row (#2070).
///
/// Content drift is not swallowed: [`slot_content_drift`] still names it and
/// [`ensure_slot_set_immutable`] still reports it, because the same bytes
/// producing a different vector is a real finding about a lens' determinism.
/// It is reported as what it is instead of refused as something it is not.
fn slot_set_difference(existing: &Constellation, incoming: &Constellation) -> Option<String> {
    let stored: BTreeSet<SlotId> = existing.slots.keys().copied().collect();
    let proposed: BTreeSet<SlotId> = incoming.slots.keys().copied().collect();
    let added = proposed.difference(&stored).copied().collect::<Vec<_>>();
    let removed = stored.difference(&proposed).copied().collect::<Vec<_>>();
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    Some(format!(
        "slots_added=[{}] slots_removed=[{}] stored_slots=[{}] proposed_slots=[{}]",
        join_slots(&added),
        join_slots(&removed),
        join_slots(&stored.iter().copied().collect::<Vec<_>>()),
        join_slots(&proposed.iter().copied().collect::<Vec<_>>()),
    ))
}

/// The slot ids present in both rows whose measured content differs.
///
/// Empty on the overwhelmingly common idempotent re-write, so this costs a
/// key-wise comparison and allocates nothing in the normal case.
fn slot_content_drift(existing: &Constellation, incoming: &Constellation) -> Vec<SlotId> {
    existing
        .slots
        .iter()
        .filter(|(slot, stored)| {
            incoming
                .slots
                .get(*slot)
                .is_some_and(|proposed| proposed != *stored)
        })
        .map(|(slot, _)| *slot)
        .collect()
}

fn join_slots(slots: &[SlotId]) -> String {
    slots
        .iter()
        .map(SlotId::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Refuses a re-measurement that would change the stored row's slot **layout**.
///
/// The stored row's Base slot membership is the row's declared shape, and the
/// qualified slot-write path (`slot_backfill.rs`) already refuses to attach a
/// slot a Base row never declared. Before #1903 the ingest path refused the same
/// thing but reported it as `CALYX_ASTER_CORRUPT_SHARD` naming only the word
/// "slots", which sends the operator to shard restore for what is a
/// panel-generation migration.
///
/// **#2070**: a re-measure that keeps every slot id and changes a lane's
/// measured content is admitted, and the drifting lanes are reported rather than
/// refused. The caller's merge keeps the **stored** vectors — first write wins
/// on content, exactly as `merge_observation_anchors` already keeps first-write
/// metadata, scalars and pointer — so admitting the write cannot leave a Base
/// row referring to slot-CF vectors that were never committed. What the write
/// achieves is the anchor merge and the temporal metadata it was made for; what
/// it deliberately does not do is silently rewrite measured history.
fn ensure_slot_set_immutable(existing: &Constellation, incoming: &Constellation) -> Result<()> {
    if let Some(difference) = slot_set_difference(existing, incoming) {
        return Err(CalyxError::aster_panel_slot_set_immutable(format!(
            "re-measured constellation {} (panel_version={}) declares a different slot set than the stored row: {difference}",
            incoming.cx_id, incoming.panel_version
        )));
    }
    let drift = slot_content_drift(existing, incoming);
    if !drift.is_empty() {
        // Loud, and non-fatal. The same source bytes at the same panel version
        // reaching a different vector means a lens is not a pure function of the
        // row it measures, which is worth an operator's attention — but it is a
        // determinism finding about the lens, not damage to the shard, and it
        // must never again be able to pin a 174,993-row backlog at page 1.
        tracing::warn!(
            code = "CALYX_ASTER_PANEL_SLOT_CONTENT_DRIFT",
            cx_id = %incoming.cx_id,
            panel_version = incoming.panel_version,
            slots_drifted = %join_slots(&drift),
            stored_slots = %join_slots(&existing.slots.keys().copied().collect::<Vec<_>>()),
            "a re-measurement of a stored row produced different content for one or more slots \
             with identical slot membership; the stored vectors are retained and the write is \
             admitted, because the frozen contract of a panel generation is its slot layout and \
             not the bytes measured into it"
        );
    }
    Ok(())
}

pub(super) fn merge_duplicate_anchors(
    existing: &mut Constellation,
    incoming: &Constellation,
) -> Result<Vec<Anchor>> {
    ensure_slot_set_immutable(existing, incoming)?;
    let identity_differences = anchor_merge_identity_differences(existing, incoming);
    if !identity_differences.is_empty() {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "CxId collision or non-idempotent duplicate constellation; differing identity fields: {}",
            identity_differences.join(", ")
        )));
    }
    if let AnchorConflictResult::Conflicting {
        anchor_type,
        reason,
    } = check_anchor_conflict(incoming, existing)
    {
        // #2072: the same misclassification as the Anchors CF batch path. Two
        // observations disagreeing about one axis is a write conflict on intact
        // bytes; sending the operator to `restore from restic/snapshot` for it
        // both destroys healthy data and buries the code that must never be
        // noise.
        return Err(CalyxError::aster_anchor_value_conflict(format!(
            "CxId duplicate has conflicting {anchor_type:?} anchor: {reason:?}; nothing is \
             corrupt — reconcile the two observations at their writers"
        )));
    }

    let mut existing_kinds = existing
        .anchors
        .iter()
        .map(|anchor| anchor.kind.clone())
        .collect::<BTreeSet<_>>();
    let mut added = Vec::new();
    for anchor in &incoming.anchors {
        if existing_kinds.insert(anchor.kind.clone()) {
            existing.anchors.push(anchor.clone());
            added.push(anchor.clone());
        }
    }
    if !added.is_empty() {
        existing.flags.ungrounded = existing.anchors.is_empty();
        existing.validate_schema()?;
    }
    Ok(added)
}

/// Merges anchors for a repeated content observation while retaining the
/// authoritative first-write metadata, scalars, pointer, and derived state.
/// The fields that prove the content identity itself still fail closed.
pub(super) fn merge_observation_anchors(
    existing: &mut Constellation,
    incoming: &Constellation,
) -> Result<Vec<Anchor>> {
    ensure_slot_set_immutable(existing, incoming)?;
    let mut differences = Vec::new();
    if existing.cx_id != incoming.cx_id {
        differences.push("cx_id");
    }
    if existing.vault_id != incoming.vault_id {
        differences.push("vault_id");
    }
    if existing.panel_version != incoming.panel_version {
        differences.push("panel_version");
    }
    if existing.input_ref.hash != incoming.input_ref.hash {
        differences.push("input_hash");
    }
    if existing.input_ref.redacted != incoming.input_ref.redacted {
        differences.push("input_redaction");
    }
    if existing.modality != incoming.modality {
        differences.push("modality");
    }
    // Slots are checked by `ensure_slot_set_immutable` above, which names the
    // exact per-slot difference and the migration that resolves it. Repeating
    // the bare "slots" difference here could only shadow that with a less
    // actionable message.
    if !differences.is_empty() {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "content-addressed observation collision; differing identity fields: {}",
            differences.join(", ")
        )));
    }
    let mut proposal = existing.clone();
    proposal.anchors = incoming.anchors.clone();
    proposal.flags.ungrounded = proposal.anchors.is_empty();
    merge_duplicate_anchors(existing, &proposal)
}

pub(super) fn stage_anchor_merge_rows(
    id: CxId,
    merged: &Constellation,
    added: &[Anchor],
) -> Result<Vec<WriteRow>> {
    let mut rows = Vec::with_capacity(1 + added.len());
    rows.push(WriteRow {
        cf: ColumnFamily::Base,
        key: base_key(id),
        value: encode::encode_constellation_base(merged)?,
    });
    for anchor in added {
        rows.push(WriteRow {
            cf: ColumnFamily::Anchors,
            key: anchor_key(id, &anchor.kind),
            value: encode::encode_anchor(anchor)?,
        });
    }
    Ok(rows)
}

fn normalized_anchor_identity(cx: &Constellation) -> Constellation {
    let mut normalized = cx.clone();
    normalized.anchors.clear();
    normalized.created_at = 0;
    normalized.flags.ungrounded = false;
    normalized.provenance = LedgerRef {
        seq: 0,
        hash: [0; 32],
    };
    // Recurrence is authoritative derived state written after duplicate
    // ingestion. A later duplicate must compare against the original caller
    // identity without erasing or requiring this system-owned counter.
    normalized.scalars.remove(FREQUENCY_SCALAR);
    normalized
}

fn anchor_merge_identity_differences(left: &Constellation, right: &Constellation) -> Vec<String> {
    let left = normalized_anchor_identity(left);
    let right = normalized_anchor_identity(right);
    let mut differences = Vec::new();
    if left.cx_id != right.cx_id {
        differences.push("cx_id".to_string());
    }
    if left.vault_id != right.vault_id {
        differences.push("vault_id".to_string());
    }
    if left.panel_version != right.panel_version {
        differences.push("panel_version".to_string());
    }
    if left.input_ref != right.input_ref {
        differences.push("input_ref".to_string());
    }
    if left.modality != right.modality {
        differences.push("modality".to_string());
    }
    // See `merge_duplicate_anchors`: slot differences are refused earlier by
    // `ensure_slot_set_immutable` with the exact added/removed/re-measured ids.
    if left.scalars != right.scalars {
        differences.push("scalars".to_string());
    }
    if left.metadata != right.metadata {
        differences.push("metadata".to_string());
    }
    if left.flags != right.flags {
        differences.push("flags".to_string());
    }
    differences
}
