use calyx_aster::vault::AsterVault;
use calyx_core::{CALYX_TEMPORAL_AP60_VIOLATION, Clock as CoreClock, CxId, Result, SlotId};
use serde::{Deserialize, Serialize};

use crate::error::{CALYX_SEXTANT_NO_LENSES, CALYX_SEXTANT_SLOT_MISSING, sextant_error};
use crate::fusion::FusionStrategy;
use crate::hit::Hit;
use crate::query::Query;
use crate::search::SearchEngine;

use super::recall_budget::windowed_primary_search;
use super::{
    Clock, TemporalPolicy, TimeWindow, WindowRecallPolicy, WindowRecallReport, apply_causal_gate,
    apply_temporal_boost, apply_temporal_boost_with_recurrence, filter_hits_by_window,
};

const PRIMARY_TEMPORAL_WEIGHT: f32 = 0.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemporalSearchResult {
    pub hits: Vec<Hit>,
    pub pre_boost_ranking: Vec<CxId>,
    #[serde(default)]
    pub windowed_ranking: Vec<CxId>,
    pub policy_snapshot: TemporalPolicy,
    pub temporal_weight_used: f32,
    #[serde(default)]
    pub primary_slots_used: Vec<SlotId>,
    #[serde(default)]
    pub temporal_slots_excluded: Vec<SlotId>,
    #[serde(default)]
    pub window_recall: WindowRecallReport,
}

pub fn temporal_search(
    engine: &SearchEngine,
    query: &Query,
    window: Option<TimeWindow>,
    policy: &TemporalPolicy,
    clock: &dyn Clock,
    tz_offset_secs: i32,
) -> Result<TemporalSearchResult> {
    temporal_search_with_recall(
        engine,
        query,
        window,
        policy,
        clock,
        tz_offset_secs,
        WindowRecallPolicy::default(),
    )
}

pub fn temporal_search_with_recall(
    engine: &SearchEngine,
    query: &Query,
    window: Option<TimeWindow>,
    policy: &TemporalPolicy,
    clock: &dyn Clock,
    tz_offset_secs: i32,
    recall_policy: WindowRecallPolicy,
) -> Result<TemporalSearchResult> {
    let input = primary_retrieval(
        engine,
        query,
        window,
        policy,
        clock,
        tz_offset_secs,
        recall_policy,
    )?;
    temporal_search_from_primary(input)
}

pub fn temporal_search_with_recurrence<C>(
    engine: &SearchEngine,
    query: &Query,
    window: Option<TimeWindow>,
    policy: &TemporalPolicy,
    clock: &dyn Clock,
    tz_offset_secs: i32,
    vault: &AsterVault<C>,
) -> Result<TemporalSearchResult>
where
    C: CoreClock,
{
    temporal_search_with_recurrence_and_recall(
        engine,
        query,
        window,
        policy,
        clock,
        tz_offset_secs,
        vault,
        WindowRecallPolicy::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn temporal_search_with_recurrence_and_recall<C>(
    engine: &SearchEngine,
    query: &Query,
    window: Option<TimeWindow>,
    policy: &TemporalPolicy,
    clock: &dyn Clock,
    tz_offset_secs: i32,
    vault: &AsterVault<C>,
    recall_policy: WindowRecallPolicy,
) -> Result<TemporalSearchResult>
where
    C: CoreClock,
{
    let input = primary_retrieval(
        engine,
        query,
        window,
        policy,
        clock,
        tz_offset_secs,
        recall_policy,
    )?;
    temporal_search_from_primary_with_recurrence(input, vault)
}

/// Shared AP-60 primary retrieval: validates primary slots, then runs the
/// explicit window-recall policy (no silent k expansion).
fn primary_retrieval<'a>(
    engine: &SearchEngine,
    query: &Query,
    window: Option<TimeWindow>,
    policy: &'a TemporalPolicy,
    clock: &'a dyn Clock,
    tz_offset_secs: i32,
    recall_policy: WindowRecallPolicy,
) -> Result<TemporalSearchInput<'a>> {
    let selected_slots = if query.slots.is_empty() {
        engine.indexes.slots()
    } else {
        query.slots.clone()
    };
    let (primary_slots, temporal_slots_excluded) = split_primary_slots(engine, &selected_slots);
    if primary_slots.is_empty() {
        return Err(sextant_error(
            CALYX_SEXTANT_NO_LENSES,
            "AP-60 temporal search has no non-temporal primary slot to query",
        ));
    }
    ensure_slots_registered(engine, &primary_slots)?;
    validate_primary_fusion(query, &primary_slots)?;

    let (primary_hits, window_recall) = if primary_slots_all_empty(engine, &primary_slots) {
        (Vec::new(), WindowRecallReport::default())
    } else {
        let mut primary_query = query.clone();
        primary_query.slots = primary_slots.clone();
        primary_query.fusion = normalized_primary_fusion(query, &primary_slots);
        windowed_primary_search(
            engine,
            &primary_query,
            window.as_ref(),
            query.k,
            recall_policy,
        )?
    };

    Ok(TemporalSearchInput {
        primary_hits,
        temporal_weight_used: PRIMARY_TEMPORAL_WEIGHT,
        final_k: query.k,
        window,
        policy,
        clock,
        tz_offset_secs,
        primary_slots_used: primary_slots,
        temporal_slots_excluded,
        window_recall,
    })
}

pub struct TemporalSearchInput<'a> {
    pub primary_hits: Vec<Hit>,
    pub temporal_weight_used: f32,
    pub final_k: usize,
    pub window: Option<TimeWindow>,
    pub policy: &'a TemporalPolicy,
    pub clock: &'a dyn Clock,
    pub tz_offset_secs: i32,
    pub primary_slots_used: Vec<SlotId>,
    pub temporal_slots_excluded: Vec<SlotId>,
    pub window_recall: WindowRecallReport,
}

pub fn temporal_search_from_primary(
    input: TemporalSearchInput<'_>,
) -> Result<TemporalSearchResult> {
    temporal_search_from_primary_inner(input, apply_temporal_boost)
}

pub fn temporal_search_from_primary_with_recurrence<C>(
    input: TemporalSearchInput<'_>,
    vault: &AsterVault<C>,
) -> Result<TemporalSearchResult>
where
    C: CoreClock,
{
    temporal_search_from_primary_inner(input, |hits, policy, query_time_secs, tz_offset_secs| {
        apply_temporal_boost_with_recurrence(hits, policy, query_time_secs, tz_offset_secs, vault)
    })
}

fn temporal_search_from_primary_inner(
    input: TemporalSearchInput<'_>,
    boost: impl FnOnce(Vec<Hit>, &TemporalPolicy, i64, i32) -> Result<Vec<Hit>>,
) -> Result<TemporalSearchResult> {
    validate_primary_temporal_weight(input.temporal_weight_used)?;
    let pre_boost_ranking = input
        .primary_hits
        .iter()
        .map(|hit| hit.cx_id)
        .collect::<Vec<_>>();
    let window = input.window.unwrap_or_else(TimeWindow::all);
    let filtered = filter_hits_by_window(input.primary_hits, &window);
    let windowed_ranking = filtered.iter().map(|hit| hit.cx_id).collect::<Vec<_>>();
    let boosted = boost(
        filtered,
        input.policy,
        input.clock.now_secs(),
        input.tz_offset_secs,
    )?;
    let mut hits = apply_causal_gate(boosted, &input.policy.boost)?;
    hits.retain(|hit| hit.score > 0.0);
    hits.truncate(input.final_k);
    for (index, hit) in hits.iter_mut().enumerate() {
        hit.rank = index + 1;
    }

    Ok(TemporalSearchResult {
        hits,
        pre_boost_ranking,
        windowed_ranking,
        policy_snapshot: *input.policy,
        temporal_weight_used: input.temporal_weight_used,
        primary_slots_used: input.primary_slots_used,
        temporal_slots_excluded: input.temporal_slots_excluded,
        window_recall: input.window_recall,
    })
}

pub fn validate_primary_temporal_weight(temporal_weight_used: f32) -> Result<()> {
    if temporal_weight_used.is_finite() && temporal_weight_used == PRIMARY_TEMPORAL_WEIGHT {
        return Ok(());
    }
    Err(sextant_error(
        CALYX_TEMPORAL_AP60_VIOLATION,
        "AP-60 requires temporal_weight_used == 0.0 in primary retrieval",
    ))
}

fn split_primary_slots(engine: &SearchEngine, slots: &[SlotId]) -> (Vec<SlotId>, Vec<SlotId>) {
    slots
        .iter()
        .copied()
        .partition(|slot| !engine.is_retrieval_only_slot(*slot))
}

fn ensure_slots_registered(engine: &SearchEngine, slots: &[SlotId]) -> Result<()> {
    let stats = engine.indexes.stats();
    for slot in slots {
        if !stats.iter().any(|stats| stats.slot == *slot) {
            return Err(sextant_error(
                CALYX_SEXTANT_SLOT_MISSING,
                format!("slot {slot} is not registered"),
            ));
        }
    }
    Ok(())
}

fn primary_slots_all_empty(engine: &SearchEngine, slots: &[SlotId]) -> bool {
    let stats = engine.indexes.stats();
    slots.iter().all(|slot| {
        stats
            .iter()
            .find(|stats| stats.slot == *slot)
            .is_some_and(|stats| stats.len == 0)
    })
}

fn validate_primary_fusion(query: &Query, primary_slots: &[SlotId]) -> Result<()> {
    let Some(FusionStrategy::SingleLens { slot }) = query.fusion else {
        return Ok(());
    };
    if primary_slots.contains(&slot) {
        return Ok(());
    }
    Err(sextant_error(
        CALYX_TEMPORAL_AP60_VIOLATION,
        format!("single-lens primary retrieval requested temporal-only slot {slot}"),
    ))
}

fn normalized_primary_fusion(query: &Query, primary_slots: &[SlotId]) -> Option<FusionStrategy> {
    match &query.fusion {
        Some(FusionStrategy::SingleLens { slot }) if primary_slots.contains(slot) => {
            query.fusion.clone()
        }
        Some(FusionStrategy::SingleLens { .. }) => None,
        Some(_) => query.fusion.clone(),
        None if primary_slots.len() == 1 => Some(FusionStrategy::SingleLens {
            slot: primary_slots[0],
        }),
        None => None,
    }
}
