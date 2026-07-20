//! Pipeline strategy helpers.

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{CxId, LedgerRef, Result, SlotId};

use super::FusionContext;
use super::rrf::rrf_fuse_restricted;
use crate::hit::Hit;
use crate::index::IndexSearchHit;

#[derive(Clone, Debug, PartialEq)]
pub struct PipelineOutput {
    pub stage1_candidates: usize,
    pub final_hits: usize,
    pub subset_ok: bool,
    pub candidate_ids: Vec<CxId>,
}

pub fn pipeline_fuse(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    if context.stage1_slots.is_empty() {
        return Err(crate::error::sextant_error(
            crate::error::CALYX_SEXTANT_RERANKER_NO_CANDIDATES,
            "pipeline fusion requires at least one declared stage-1 slot",
        ));
    }
    let candidates = stage1_candidates(results, &context.stage1_slots);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let scoring_results = non_stage1_results(results, &context.stage1_slots);
    if scoring_results.is_empty() {
        return Err(crate::error::sextant_error(
            crate::error::CALYX_SEXTANT_RERANKER_NO_CANDIDATES,
            "pipeline fusion has no non-stage-1 scoring index",
        ));
    }
    rrf_fuse_restricted(&scoring_results, context, &candidates, provenance)
}

pub fn summarize_pipeline(stage1: &[CxId], final_ids: &[CxId]) -> PipelineOutput {
    PipelineOutput {
        stage1_candidates: stage1.len(),
        final_hits: final_ids.len(),
        subset_ok: final_ids.iter().all(|cx| stage1.contains(cx)),
        candidate_ids: final_ids.to_vec(),
    }
}

fn stage1_candidates(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    stage1_slots: &[SlotId],
) -> BTreeSet<CxId> {
    stage1_slots
        .iter()
        .filter_map(|slot| results.get(slot))
        .flat_map(|hits| hits.iter().map(|hit| hit.cx_id))
        .collect()
}

fn non_stage1_results(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    stage1_slots: &[SlotId],
) -> BTreeMap<SlotId, Vec<IndexSearchHit>> {
    results
        .iter()
        .filter(|(slot, _)| !stage1_slots.contains(slot))
        .map(|(slot, hits)| (*slot, hits.clone()))
        .collect()
}
