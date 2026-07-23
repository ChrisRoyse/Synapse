use std::collections::BTreeMap;

use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::vault::AsterVault;
use calyx_aster::{cf::ColumnFamily, vault::encode::decode_constellation_base};
use calyx_core::{Clock, Constellation, CxId, SlotVector};
use calyx_sextant::{FreshnessTag, Hit};

use super::{SEARCH_READER_LEASE_MS, SearchFreshness};
use crate::error::CliResult;
use crate::persisted::PersistedSearchIndexes;

pub(super) fn index_freshness_tag(
    indexes: &PersistedSearchIndexes,
    pinned_seq: u64,
    derived_content_seq: u64,
    freshness: SearchFreshness,
) -> CliResult<FreshnessTag> {
    match freshness {
        SearchFreshness::Fresh => {
            indexes.ensure_fresh_at_snapshot(pinned_seq, derived_content_seq)?;
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
    pub(super) fn pin(vault: &'a AsterVault<C>) -> Self {
        Self {
            vault,
            snapshot: vault.pin_reader(Freshness::FreshDerived, SEARCH_READER_LEASE_MS),
        }
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        self.snapshot
    }

    pub(super) fn seq(&self) -> u64 {
        self.snapshot.seq()
    }

    /// Derived-content watermark observed at pin time, clamped to the pinned
    /// seq by the MVCC store (issue #1100).
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
