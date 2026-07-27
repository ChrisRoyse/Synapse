use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, Constellation, CxId};
use calyx_sextant::{FreshnessTag, Hit};

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;
use crate::provenance::hit_docs_at;

use super::SearchBudget;
use super::hydration_cache;
use super::support::SearchReadSnapshot;

#[allow(clippy::too_many_arguments)]
pub(super) fn hydrate_hit_docs_with_bounded_readbacks<C: Clock>(
    vault: &AsterVault<C>,
    vault_dir: &Path,
    hits: &[Hit],
    hydrate_hit_slots: bool,
    read: &SearchReadSnapshot<'_, C>,
    freshness_tag: FreshnessTag,
    trace: &mut SearchTracer<'_>,
    budget: &mut SearchBudget<'_>,
) -> CliResult<(BTreeMap<CxId, Constellation>, FreshnessTag)> {
    if hits.is_empty() {
        budget.check("empty_hit_set", 0)?;
        return Ok((BTreeMap::new(), freshness_tag));
    }

    let mut docs = BTreeMap::new();
    for (hit_index, hit) in hits.iter().enumerate() {
        budget.check("before_hit_doc_hydration", hit_index)?;
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
            .map_err(|error| contextualize_hit_hydration_error(error, hit, hit_index, read))?;
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
    Ok((docs, freshness_tag))
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
