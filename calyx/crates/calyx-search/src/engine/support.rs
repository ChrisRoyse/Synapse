use std::collections::{BTreeMap, BTreeSet};

use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::vault::AsterVault;
use calyx_aster::{cf::ColumnFamily, vault::encode::decode_constellation_base};
use calyx_core::{CalyxError, Clock, Constellation, CxId, Panel, SlotId, SlotState, SlotVector};
use calyx_sextant::{FreshnessTag, Hit};

use super::{SEARCH_READER_LEASE_MS, SearchFreshness};
use crate::error::CliResult;
use crate::persisted::{PersistedSearchGeneration, PersistedSearchIndexes};

/// Proves that an immutable generation advertises only slots the exact panel
/// contract permits for primary retrieval.
///
/// This is deliberately checked on every open, not merely during rebuild. A
/// daemon may be upgraded while an older manifest remains on disk; accepting
/// that manifest would let a post-retrieval ordinate participate in recall or
/// defer the defect until weighted fusion fails on the first matching query.
pub(super) fn validate_generation_panel_contract(
    generation: &PersistedSearchGeneration,
    panel: &Panel,
) -> CliResult<()> {
    if generation.panel_version != panel.version {
        return Err(CalyxError::stale_derived(format!(
            "search generation panel {} does not match active panel {}; rebuild the exact panel generation",
            generation.panel_version, panel.version
        ))
        .into());
    }

    let mut seen = BTreeSet::new();
    for persisted in &generation.slots {
        let slot_id = persisted.panel_slot.slot_id();
        if !seen.insert(slot_id) {
            return Err(CalyxError::stale_derived(format!(
                "search generation {} declares slot {slot_id} more than once; rebuild the corrupt generation",
                generation.manifest_sha256
            ))
            .into());
        }
        let declared = panel
            .slots
            .iter()
            .find(|candidate| candidate.slot_id == slot_id)
            .ok_or_else(|| {
                CalyxError::stale_derived(format!(
                    "search generation {} indexes undeclared slot {slot_id} for panel {}; rebuild from the exact panel contract",
                    generation.manifest_sha256, panel.version
                ))
            })?;
        if declared.state != SlotState::Active {
            return Err(CalyxError::stale_derived(format!(
                "search generation {} indexes panel {} slot {slot_id}, but its declared state is {:?}; rebuild after the lifecycle change",
                generation.manifest_sha256, panel.version, declared.state
            ))
            .into());
        }
        if declared.retrieval_only {
            return Err(CalyxError::stale_derived(format!(
                "search generation {} indexes retrieval-only panel {} slot {slot_id}; this slot is a post-retrieval ordinate and must not participate in primary similarity search; rebuild from the exact panel contract",
                generation.manifest_sha256, panel.version
            ))
            .into());
        }
        if persisted.shape != declared.shape {
            return Err(CalyxError::stale_derived(format!(
                "search generation {} slot {slot_id} shape {:?} differs from panel {} contract {:?}; rebuild from the exact panel contract",
                generation.manifest_sha256, persisted.shape, panel.version, declared.shape
            ))
            .into());
        }
    }
    Ok(())
}

/// Rejects caller-provided primary query vectors that address a slot the panel
/// does not permit for similarity retrieval.
pub(super) fn validate_primary_query_slots(
    panel: &Panel,
    query_vectors: &[(SlotId, SlotVector)],
) -> CliResult<()> {
    let mut seen = BTreeSet::new();
    for (slot_id, _) in query_vectors {
        if !seen.insert(*slot_id) {
            return Err(CalyxError {
                code: calyx_sextant::error::CALYX_SEXTANT_QUERY_SHAPE,
                message: format!(
                    "primary query names panel {} slot {slot_id} more than once",
                    panel.version
                ),
                remediation: "supply exactly one query vector per active searchable panel slot",
            }
            .into());
        }
        let declared = panel
            .slots
            .iter()
            .find(|candidate| candidate.slot_id == *slot_id)
            .ok_or_else(|| CalyxError {
                code: calyx_sextant::error::CALYX_SEXTANT_SLOT_MISSING,
                message: format!(
                    "primary query names slot {slot_id}, which panel {} does not declare",
                    panel.version
                ),
                remediation: "supply query vectors only for slots declared by the exact active panel",
            })?;
        if declared.state != SlotState::Active {
            return Err(CalyxError {
                code: calyx_sextant::error::CALYX_SEXTANT_SLOT_INACTIVE,
                message: format!(
                    "primary query names panel {} slot {slot_id}, whose state is {:?}",
                    panel.version, declared.state
                ),
                remediation: "supply query vectors only for active searchable panel slots",
            }
            .into());
        }
        if declared.retrieval_only {
            return Err(CalyxError {
                code: calyx_sextant::error::CALYX_SEXTANT_QUERY_SHAPE,
                message: format!(
                    "primary query names retrieval-only panel {} slot {slot_id}; it is a post-retrieval ordinate, not a similarity lane",
                    panel.version
                ),
                remediation: "remove retrieval-only ordinates from primary query vectors and apply them only in the declared post-retrieval stage",
            }
            .into());
        }
    }
    Ok(())
}

pub(super) fn index_freshness_tag(
    indexes: &PersistedSearchIndexes,
    pinned_seq: u64,
    derived_content_seq: u64,
    freshness: SearchFreshness,
    reconciled_to_seq: Option<u64>,
) -> CliResult<FreshnessTag> {
    match freshness {
        SearchFreshness::Fresh => {
            if derived_content_seq > pinned_seq || indexes.base_seq() > pinned_seq {
                indexes.ensure_fresh_at_snapshot(pinned_seq, derived_content_seq)?;
            }
            if derived_content_seq > indexes.base_seq() {
                if reconciled_to_seq != Some(pinned_seq) {
                    indexes.ensure_fresh_at_snapshot(pinned_seq, derived_content_seq)?;
                }
                return Ok(FreshnessTag::fresh_reconciled(
                    indexes.base_seq(),
                    pinned_seq,
                ));
            }
            Ok(FreshnessTag::fresh(pinned_seq))
        }
        SearchFreshness::StaleOk => {
            let built_at_seq = indexes.base_seq();
            if built_at_seq > pinned_seq {
                return Err(calyx_core::CalyxError::stale_derived(format!(
                    "persistent search manifest base seq {built_at_seq} is ahead of pinned vault seq {pinned_seq}; rebuild the vault search indexes before search"
                ))
                .into());
            }
            Ok(FreshnessTag::stale_ok(built_at_seq, pinned_seq))
        }
    }
}

pub(super) struct SearchReadSnapshot<'a, C: Clock> {
    vault: &'a AsterVault<C>,
    snapshot: Snapshot,
}

impl<'a, C: Clock> SearchReadSnapshot<'a, C> {
    pub(super) fn pin(vault: &'a AsterVault<C>, panel_version: u32) -> CliResult<Self> {
        Ok(Self {
            vault,
            snapshot: vault.pin_reader_for_panel(
                panel_version,
                Freshness::FreshDerived,
                SEARCH_READER_LEASE_MS,
            )?,
        })
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        self.snapshot
    }

    pub(super) fn seq(&self) -> u64 {
        self.snapshot.seq()
    }

    /// Exact-panel content watermark observed atomically with the pinned seq.
    pub(super) fn derived_content_seq(&self) -> u64 {
        self.snapshot.derived_content_seq()
    }

    pub(super) fn lease_id(&self) -> u64 {
        self.snapshot.lease().id()
    }

    pub(super) fn lease_max_age_ms(&self) -> u64 {
        self.snapshot.lease().max_age_ms()
    }

    pub(super) fn lease_expires_at(&self) -> u64 {
        self.snapshot.lease().expires_at()
    }
}

impl<C: Clock> Drop for SearchReadSnapshot<'_, C> {
    fn drop(&mut self) {
        let _ = self.vault.release_reader(self.snapshot.lease().id());
    }
}

pub(super) fn is_stale_derived(error: &crate::error::SearchError) -> bool {
    matches!(error, crate::error::SearchError::Calyx(inner) if inner.code == "CALYX_STALE_DERIVED")
}

pub(super) fn vault_base_count_at<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    panel_version: u32,
) -> CliResult<usize> {
    vault
        .scan_cf_snapshot(snapshot, ColumnFamily::Base)?
        .into_iter()
        .try_fold(0usize, |count, (_, bytes)| {
            let cx = decode_constellation_base(&bytes)?;
            Ok(count + usize::from(cx.panel_version == panel_version))
        })
}

pub(super) fn renumber_and_truncate(hits: &mut Vec<Hit>, k: usize) {
    hits.truncate(k);
    for (idx, hit) in hits.iter_mut().enumerate() {
        hit.rank = idx + 1;
    }
}

pub(super) fn cosine(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let (mut dot, mut l2, mut r2) = (0.0f32, 0.0f32, 0.0f32);
    for (l, r) in left.iter().zip(right) {
        dot += l * r;
        l2 += l * l;
        r2 += r * r;
    }
    (l2 > 0.0 && r2 > 0.0).then(|| dot / (l2.sqrt() * r2.sqrt()))
}

pub(super) fn guard_cosine(
    hit: &Hit,
    docs: &BTreeMap<CxId, Constellation>,
    query_vectors: &[(calyx_core::SlotId, SlotVector)],
) -> Option<f32> {
    let cx = docs.get(&hit.cx_id)?;
    hit.per_lens
        .iter()
        .filter_map(|item| {
            let query = query_vectors
                .iter()
                .find(|(slot, _)| *slot == item.slot.slot_id())?
                .1
                .as_dense()?;
            if cx.panel_version != item.slot.panel_version() {
                return None;
            }
            let doc = cx.slots.get(&item.slot.slot_id())?.as_dense()?;
            cosine(query, doc)
        })
        .max_by(f32::total_cmp)
}
