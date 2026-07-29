use std::collections::BTreeMap;
use std::collections::BTreeSet;

use calyx_core::{CxId, LedgerRef, PanelSlotId, Result, SlotId};

use super::FusionContext;
use crate::hit::{FreshnessTag, Hit, PerLensContribution, ProvenanceSource};
use crate::index::IndexSearchHit;

/// One lens's Reciprocal Rank Fusion contribution at `rank`, scored with the
/// `rrf_k` the query is actually running under.
///
/// `rrf_k` is supplied rather than hardcoded so a tuned vault, its reproduce
/// path and its reported formula cannot disagree (issue #1883). The workspace
/// default is `calyx_core::RRF_K_DEFAULT`.
pub fn rrf_contribution(weight: f32, rank: usize, rrf_k: f32) -> f32 {
    weight / (rank as f32 + rrf_k)
}

pub fn rrf_fuse(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    let weights = results.keys().map(|slot| (*slot, 1.0)).collect();
    fuse_with_weights(results, context, &weights, 1.0, provenance)
}

pub fn rrf_fuse_restricted(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    candidates: &BTreeSet<CxId>,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    let filtered = results
        .iter()
        .map(|(slot, hits)| {
            (
                *slot,
                hits.iter()
                    .filter(|hit| candidates.contains(&hit.cx_id))
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    rrf_fuse(&filtered, context, provenance)
}

pub fn weighted_rrf_fuse(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    fuse_with_weights(results, context, &context.weights, 0.0, provenance)
}

fn fuse_with_weights(
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    weights: &BTreeMap<SlotId, f32>,
    default_weight: f32,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    let mut fused = BTreeMap::<CxId, (f32, Vec<PerLensContribution>)>::new();
    for (slot, hits) in results {
        let weight = *weights.get(slot).unwrap_or(&default_weight);
        if weight <= 0.0 {
            continue;
        }
        for hit in hits {
            let contribution = rrf_contribution(weight, hit.rank, context.rrf_k);
            let entry = fused.entry(hit.cx_id).or_default();
            entry.0 += contribution;
            entry.1.push(PerLensContribution {
                slot: PanelSlotId::new(context.panel_version, *slot),
                rank: hit.rank,
                raw_score: hit.score,
                weight,
                contribution,
            });
        }
    }
    let mut rows: Vec<_> = fused.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.0
            .total_cmp(&a.1.0)
            .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
    });
    rows.truncate(context.k);
    rows.into_iter()
        .enumerate()
        .map(|(idx, (cx_id, (score, per_lens)))| {
            let mut hit = Hit {
                cx_id,
                score,
                rank: idx + 1,
                event_time_secs: None,
                temporal_scores: None,
                causal_confidence: crate::temporal::CausalConfidence::Absent,
                causal_gate: None,
                per_lens,
                cross_terms_used: false,
                guard: None,
                provenance: provenance(cx_id)?,
                provenance_source: ProvenanceSource::Stored,
                freshness: FreshnessTag::fresh(0),
                explain: None,
            };
            if context.explain {
                hit = hit.with_explain(context.strategy.name());
            }
            Ok(hit)
        })
        .collect()
}
