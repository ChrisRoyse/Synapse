use std::collections::BTreeMap;

use calyx_core::{CxId, LedgerRef, PanelSlotId, Result, SlotId};

use super::FusionContext;
use crate::hit::{FreshnessTag, Hit, PerLensContribution, ProvenanceSource};
use crate::index::IndexSearchHit;

pub fn single_lens_fuse(
    slot: SlotId,
    results: &BTreeMap<SlotId, Vec<IndexSearchHit>>,
    context: &FusionContext,
    provenance: &dyn Fn(CxId) -> Result<LedgerRef>,
) -> Result<Vec<Hit>> {
    let Some(items) = results.get(&slot) else {
        return Ok(Vec::new());
    };
    let mut hits: Vec<_> = items
        .iter()
        .take(context.k)
        .map(|item| {
            Ok(Hit {
                cx_id: item.cx_id,
                score: item.score,
                rank: item.rank,
                event_time_secs: None,
                temporal_scores: None,
                causal_confidence: crate::temporal::CausalConfidence::Absent,
                causal_gate: None,
                per_lens: vec![PerLensContribution {
                    slot: PanelSlotId::new(context.panel_version, slot),
                    rank: item.rank,
                    raw_score: item.score,
                    weight: 1.0,
                    contribution: item.score,
                }],
                cross_terms_used: false,
                guard: None,
                provenance: provenance(item.cx_id)?,
                provenance_source: ProvenanceSource::Stored,
                freshness: FreshnessTag::fresh(0),
                explain: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if context.explain {
        for hit in &mut hits {
            *hit = hit.clone().with_explain("single_lens");
        }
    }
    Ok(hits)
}
