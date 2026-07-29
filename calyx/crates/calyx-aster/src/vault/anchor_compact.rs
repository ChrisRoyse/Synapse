use std::collections::BTreeSet;

use calyx_core::{Anchor, AnchorKind, AnchorValue, Clock, Constellation, CxId, Result, VaultStore};

use super::{AsterVault, encode};
use crate::cf::{ColumnFamily, anchor_key, base_key};
use crate::dedup::{AnchorConflictResult, check_anchor_conflict};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnchorCompactionReport {
    pub scanned: usize,
    pub compacted: usize,
    pub removed_duplicates: usize,
    pub conflicts: Vec<AnchorCompactionConflict>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnchorCompactionConflict {
    pub cx_id: CxId,
    pub anchor_kind: AnchorKind,
    pub existing_value: AnchorValue,
    pub incoming_value: AnchorValue,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    pub fn compact_duplicate_anchors(&self) -> Result<AnchorCompactionReport> {
        self.with_durable_commit_lock(|| {
            let snapshot = self.snapshot();
            let mut report = AnchorCompactionReport::default();
            let mut rows = Vec::new();
            for (_, bytes) in self.scan_cf_at(snapshot, ColumnFamily::Base)? {
                report.scanned += 1;
                // Carries the stored slot hashes through the rewrite (#1888).
                let mut rewrite = super::base_rewrite::BaseRowRewrite::decode(&bytes)?;
                let (deduped, conflicts) = dedup_anchors(rewrite.constellation());
                if !conflicts.is_empty() {
                    report.conflicts.extend(conflicts);
                    continue;
                }
                if deduped.len() == rewrite.constellation().anchors.len() {
                    continue;
                }
                report.compacted += 1;
                report.removed_duplicates += rewrite.constellation().anchors.len() - deduped.len();
                let constellation = rewrite.constellation_mut();
                constellation.anchors = deduped;
                constellation.flags.ungrounded = constellation.anchors.is_empty();
                let constellation = rewrite.constellation().clone();
                rows.push(encode::WriteRow {
                    cf: ColumnFamily::Base,
                    key: base_key(constellation.cx_id),
                    value: rewrite.encode()?,
                });
                for anchor in &constellation.anchors {
                    rows.push(encode::WriteRow {
                        cf: ColumnFamily::Anchors,
                        key: anchor_key(constellation.cx_id, &anchor.kind),
                        value: encode::encode_anchor(anchor)?,
                    });
                }
            }
            if !rows.is_empty() {
                self.commit_rows_locked(&rows)?;
            }
            Ok(report)
        })
    }
}

fn dedup_anchors(cx: &Constellation) -> (Vec<Anchor>, Vec<AnchorCompactionConflict>) {
    let mut seen = BTreeSet::<AnchorKind>::new();
    let mut deduped = Vec::<Anchor>::with_capacity(cx.anchors.len());
    let mut conflicts = Vec::new();
    for anchor in &cx.anchors {
        if let Some(existing) = deduped.iter().find(|kept| kept.kind == anchor.kind) {
            if anchors_conflict(cx, anchor, existing) {
                conflicts.push(AnchorCompactionConflict {
                    cx_id: cx.cx_id,
                    anchor_kind: existing.kind.clone(),
                    existing_value: existing.value.clone(),
                    incoming_value: anchor.value.clone(),
                });
            }
            continue;
        }
        if seen.insert(anchor.kind.clone()) {
            deduped.push(anchor.clone());
        }
    }
    (deduped, conflicts)
}

fn anchors_conflict(template: &Constellation, incoming: &Anchor, existing: &Anchor) -> bool {
    let mut incoming_cx = template.clone();
    incoming_cx.anchors = vec![incoming.clone()];
    let mut existing_cx = template.clone();
    existing_cx.anchors = vec![existing.clone()];
    matches!(
        check_anchor_conflict(&incoming_cx, &existing_cx),
        AnchorConflictResult::Conflicting { .. }
    )
}
