use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, Constellation, CxId};
use calyx_sextant::{FreshnessTag, Hit};

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;
use crate::persisted::PersistedSearchIndexes;
use crate::provenance::hit_docs_at;

use super::hydration_cache;
use super::support::{SearchReadSnapshot, index_freshness_tag};
use super::{SearchBudget, SearchFreshness};

#[allow(clippy::too_many_arguments)]
pub(super) fn hydrate_hit_docs_with_bounded_readbacks<C: Clock>(
    vault: &AsterVault<C>,
    vault_dir: &Path,
    indexes: &PersistedSearchIndexes,
    hits: &[Hit],
    freshness: SearchFreshness,
    hydrate_hit_slots: bool,
    trace: &mut SearchTracer<'_>,
    budget: &mut SearchBudget<'_>,
) -> CliResult<(BTreeMap<CxId, Constellation>, FreshnessTag)> {
    if hits.is_empty() {
        budget.check("empty_hit_set", 0)?;
        let read = pin_search_readback(
            vault,
            indexes.panel_version(),
            trace,
            "empty_hit_set",
            None,
            0,
        )?;
        let freshness_tag = verify_index_freshness(indexes, &read, freshness, trace)?;
        return Ok((BTreeMap::new(), freshness_tag));
    }

    let mut docs = BTreeMap::new();
    let mut expected_seq = None;
    let mut freshness_tag = None;
    for (hit_index, hit) in hits.iter().enumerate() {
        budget.check("before_hit_doc_hydration", hit_index)?;
        let read = pin_search_readback(
            vault,
            indexes.panel_version(),
            trace,
            "hit_doc_hydration",
            Some(hit.cx_id),
            hit_index + 1,
        )?;
        if let Some(seq) = expected_seq {
            if read.seq() != seq {
                return Err(calyx_core::CalyxError::stale_derived(format!(
                    "vault advanced during search hit hydration: initial pinned seq {seq}, \
                     hit_index={hit_index}, cx_id={}, new pinned seq {}, search index base seq {}; \
                     refusing to mix index hits with a newer Base snapshot",
                    hit.cx_id,
                    read.seq(),
                    indexes.base_seq()
                ))
                .into());
            }
        } else {
            expected_seq = Some(read.seq());
        }
        let tag = verify_index_freshness(indexes, &read, freshness, trace)?;
        if freshness_tag.is_none() {
            freshness_tag = Some(tag);
        }
        trace.emit_detail(
            "hit_doc.hydrate.start",
            None,
            Some(hit_index + 1),
            Some(format!(
                "cx_id={} snapshot_seq={} hydrate_slots={hydrate_hit_slots}",
                hit.cx_id,
                read.seq()
            )),
        );
        let slots_key = hit_slots_key(hit);
        let cached = hydration_cache::cached_doc(
            vault_dir,
            hit.cx_id,
            read.seq(),
            hydrate_hit_slots,
            &slots_key,
        )?;
        let from_cache = cached.is_some();
        if let Some(doc) = cached {
            docs.insert(hit.cx_id, (*doc).clone());
        } else {
            let one = hit_docs_at(
                vault,
                std::slice::from_ref(hit),
                read.snapshot(),
                hydrate_hit_slots,
            )
            .map_err(|error| contextualize_hit_hydration_error(error, hit, hit_index, &read))?;
            if let Some(doc) = one.get(&hit.cx_id) {
                hydration_cache::store_doc(
                    vault_dir,
                    hit.cx_id,
                    read.seq(),
                    hydrate_hit_slots,
                    &slots_key,
                    Arc::new(doc.clone()),
                )?;
            }
            docs.extend(one);
        }
        budget.check("after_hit_doc_hydration", hit_index + 1)?;
        trace.emit_detail(
            "hit_doc.hydrate.done",
            None,
            Some(hit_index + 1),
            Some(format!(
                "cx_id={} snapshot_seq={} cached={from_cache}",
                hit.cx_id,
                read.seq()
            )),
        );
    }

    let tag = freshness_tag.ok_or_else(|| {
        calyx_core::CalyxError::stale_derived(
            "search hydration produced hits but no verified freshness tag",
        )
    })?;
    Ok((docs, tag))
}

fn hit_slots_key(hit: &Hit) -> String {
    let slots = hit
        .per_lens
        .iter()
        .map(|lens_hit| lens_hit.slot)
        .collect::<BTreeSet<_>>();
    slots
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn pin_search_readback<'a, C: Clock>(
    vault: &'a AsterVault<C>,
    panel_version: u32,
    trace: &mut SearchTracer<'_>,
    phase: &'static str,
    cx_id: Option<CxId>,
    hit_ordinal: usize,
) -> CliResult<SearchReadSnapshot<'a, C>> {
    trace.emit_detail(
        "snapshot.pin.start",
        None,
        if hit_ordinal == 0 {
            None
        } else {
            Some(hit_ordinal)
        },
        Some(snapshot_detail(phase, cx_id, None)),
    );
    let read = SearchReadSnapshot::pin(vault, panel_version)?;
    trace.emit_detail(
        "snapshot.pin.done",
        None,
        Some(read.seq() as usize),
        Some(snapshot_detail(
            phase,
            cx_id,
            Some(format!(
                "lease_id={} max_age_ms={} expires_at={}",
                read.lease_id(),
                read.lease_max_age_ms(),
                read.lease_expires_at()
            )),
        )),
    );
    Ok(read)
}

fn verify_index_freshness<C: Clock>(
    indexes: &PersistedSearchIndexes,
    read: &SearchReadSnapshot<'_, C>,
    freshness: SearchFreshness,
    trace: &mut SearchTracer<'_>,
) -> CliResult<FreshnessTag> {
    let pinned_seq = read.seq();
    let derived_content_seq = read.derived_content_seq();
    trace.emit_detail(
        "indexes.freshness.start",
        None,
        None,
        Some(format!(
            "pinned_seq={pinned_seq} derived_content_seq={derived_content_seq} index_base_seq={}",
            indexes.base_seq()
        )),
    );
    let freshness_tag = index_freshness_tag(indexes, pinned_seq, derived_content_seq, freshness)?;
    trace.emit_detail(
        "indexes.freshness.done",
        None,
        None,
        Some(format!(
            "{} pinned_seq={pinned_seq} derived_content_seq={derived_content_seq} index_base_seq={}",
            freshness_tag.policy,
            indexes.base_seq()
        )),
    );
    Ok(freshness_tag)
}

fn contextualize_hit_hydration_error<C: Clock>(
    error: crate::error::SearchError,
    hit: &Hit,
    hit_index: usize,
    read: &SearchReadSnapshot<'_, C>,
) -> crate::error::SearchError {
    if error.code() != "CALYX_READER_LEASE_EXPIRED" {
        return error;
    }
    calyx_core::CalyxError::reader_lease_expired(format!(
        "reader lease expired while hydrating search hit: hit_index={hit_index}, cx_id={}, \
         snapshot_seq={}, lease_id={}, max_age_ms={}, expires_at={}",
        hit.cx_id,
        read.seq(),
        read.lease_id(),
        read.lease_max_age_ms(),
        read.lease_expires_at()
    ))
    .into()
}

fn snapshot_detail(phase: &str, cx_id: Option<CxId>, extra: Option<String>) -> String {
    match (cx_id, extra) {
        (Some(cx_id), Some(extra)) => format!("phase={phase} cx_id={cx_id} {extra}"),
        (Some(cx_id), None) => format!("phase={phase} cx_id={cx_id}"),
        (None, Some(extra)) => format!("phase={phase} {extra}"),
        (None, None) => format!("phase={phase}"),
    }
}
