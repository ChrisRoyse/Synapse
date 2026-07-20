//! Fusion replay and reproduce verdicts.

use std::collections::BTreeMap;

use calyx_core::{CalyxError, CxId, PanelSlotId, Result, SlotVector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ForgeBackend, QueryId, RemeasuredSlot, ReproduceInputResolver, ReproduceLensRegistry,
    build_reproduce_context, remeasure_slots, remeasure_slots_with_input_resolver,
};
use crate::append::{LedgerCfStore, recover_tip};
use crate::codec::encode;
use crate::entry::{ActorId, LedgerEntry, SubjectId};
use crate::head_anchor::LedgerHeadAnchor;
use crate::kind::EntryKind;
use crate::redaction::RedactionPolicy;

pub const REPRODUCE_TOLERANCE: f64 = 1.0e-3;
pub const REPRODUCE_PAYLOAD_TAG: &str = "reproduce_v1";
const RRF_K: f32 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionMode {
    SingleLens,
    Rrf,
    WeightedRrf,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotWeight {
    pub panel_slot: PanelSlotId,
    pub weight: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FusionWeights {
    pub mode: FusionMode,
    pub k: usize,
    pub candidates: Vec<CxId>,
    #[serde(default)]
    pub weights: Vec<SlotWeight>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_slot: Option<PanelSlotId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HitRef {
    pub cx_id: CxId,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReproduceResult {
    pub reproduced: bool,
    pub max_drift: f64,
    pub original_hits: Vec<HitRef>,
    pub reproduced_hits: Vec<HitRef>,
}

pub fn rerun_fusion(
    remeasured: &[RemeasuredSlot],
    fusion_weights: &FusionWeights,
) -> Result<Vec<HitRef>> {
    if fusion_weights.k == 0 || fusion_weights.candidates.is_empty() {
        return Ok(Vec::new());
    }
    let weights = weight_map(&fusion_weights.weights);
    let mut fused = BTreeMap::<CxId, f32>::new();
    for slot in remeasured {
        if !slot_participates(slot.panel_slot, fusion_weights) {
            continue;
        }
        let dense = dense_scores(&slot.vector)?;
        if dense.len() != fusion_weights.candidates.len() {
            return Err(CalyxError::ledger_corrupt(format!(
                "slot {} replay scores {} != candidates {}",
                slot.panel_slot,
                dense.len(),
                fusion_weights.candidates.len()
            )));
        }
        let weight = slot_weight(slot.panel_slot, fusion_weights, &weights);
        if weight <= 0.0 {
            continue;
        }
        for (rank, index) in ranked_indices(dense).into_iter().enumerate() {
            let cx_id = fusion_weights.candidates[index];
            let contribution = match fusion_weights.mode {
                FusionMode::SingleLens => dense[index],
                FusionMode::Rrf | FusionMode::WeightedRrf => weight / ((rank + 1) as f32 + RRF_K),
            };
            *fused.entry(cx_id).or_default() += contribution;
        }
    }
    let mut rows: Vec<_> = fused
        .into_iter()
        .map(|(cx_id, score)| HitRef { cx_id, score })
        .collect();
    rows.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.cx_id.to_string().cmp(&right.cx_id.to_string()))
    });
    rows.truncate(fusion_weights.k);
    Ok(rows)
}

pub fn assert_within_tolerance(
    original: &[HitRef],
    reproduced: &[HitRef],
    tol: f64,
) -> (bool, f64) {
    if original.is_empty() && reproduced.is_empty() {
        return (true, 0.0);
    }
    if original.len() != reproduced.len() {
        return (false, 1.0);
    }
    let reproduced_by_id: BTreeMap<_, _> = reproduced
        .iter()
        .map(|hit| (hit.cx_id, f64::from(hit.score)))
        .collect();
    let mut max_drift = 0.0_f64;
    for hit in original {
        let Some(score) = reproduced_by_id.get(&hit.cx_id) else {
            return (false, 1.0);
        };
        max_drift = max_drift.max((f64::from(hit.score) - score).abs());
    }
    (max_drift <= tol, max_drift)
}

pub fn reproduce(
    store: &mut impl LedgerCfStore,
    registry: &dyn ReproduceLensRegistry,
    forge: &mut dyn ForgeBackend,
    answer_id: &QueryId,
) -> Result<ReproduceResult> {
    let result = reproduce_verdict(store, registry, forge, answer_id)?;
    append_reproduce_entry(store, answer_id, &result)?;
    Ok(result)
}

pub fn reproduce_with_input_resolver(
    store: &mut impl LedgerCfStore,
    registry: &dyn ReproduceLensRegistry,
    forge: &mut dyn ForgeBackend,
    resolver: &dyn ReproduceInputResolver,
    answer_id: &QueryId,
) -> Result<ReproduceResult> {
    let result =
        reproduce_verdict_with_input_resolver(store, registry, forge, resolver, answer_id)?;
    append_reproduce_entry(store, answer_id, &result)?;
    Ok(result)
}

pub fn reproduce_verdict(
    store: &impl LedgerCfStore,
    registry: &dyn ReproduceLensRegistry,
    forge: &mut dyn ForgeBackend,
    answer_id: &QueryId,
) -> Result<ReproduceResult> {
    let ctx = build_reproduce_context(store, answer_id)?;
    let remeasured = remeasure_slots(&ctx, registry, forge)?;
    reproduce_result_from_remeasured(&ctx.ledger_entries, &remeasured)
}

pub fn reproduce_verdict_with_input_resolver(
    store: &impl LedgerCfStore,
    registry: &dyn ReproduceLensRegistry,
    forge: &mut dyn ForgeBackend,
    resolver: &dyn ReproduceInputResolver,
    answer_id: &QueryId,
) -> Result<ReproduceResult> {
    let ctx = build_reproduce_context(store, answer_id)?;
    let remeasured = remeasure_slots_with_input_resolver(&ctx, registry, forge, resolver)?;
    reproduce_result_from_remeasured(&ctx.ledger_entries, &remeasured)
}

pub fn append_reproduce_entry(
    store: &mut impl LedgerCfStore,
    answer_id: &QueryId,
    result: &ReproduceResult,
) -> Result<LedgerEntry> {
    let (seq, prev_hash, last_ts) = recover_tip(store)?;
    let ts = last_ts
        .checked_add(1)
        .ok_or_else(|| CalyxError::ledger_chain_broken("ledger timestamp exhausted"))?;
    let payload = reproduce_payload_bytes(answer_id, result, ts)?;
    RedactionPolicy::check_payload(&payload)?;
    let entry = LedgerEntry::new(
        seq,
        prev_hash,
        EntryKind::Admin,
        SubjectId::Query(answer_id.clone()),
        payload,
        ActorId::Service("calyx-reproduce".to_string()),
        ts,
    );
    store.put_new(seq, &encode(&entry))?;
    // Advance the external head witness exactly as `LedgerAppender::commit_prepared`
    // does. Without this the anchor stays stale, a later `LedgerAppender::open`
    // recovers `next_seq = anchor.height` and the next commit collides at this
    // reproduce row, permanently wedging an anchor-backed ledger.
    let anchor = LedgerHeadAnchor::new(seq.saturating_add(1), entry.entry_hash)?;
    store.put_head_anchor(&anchor)?;
    Ok(entry)
}

pub fn assert_reproduced(result: &ReproduceResult) -> Result<()> {
    if result.reproduced {
        Ok(())
    } else {
        Err(CalyxError::reproduce_drift_exceeded(format!(
            "max_drift {} exceeded {}",
            result.max_drift, REPRODUCE_TOLERANCE
        )))
    }
}

fn reproduce_result_from_remeasured(
    ledger_entries: &[LedgerEntry],
    remeasured: &[RemeasuredSlot],
) -> Result<ReproduceResult> {
    let payload = answer_payload(ledger_entries)?;
    let original_hits = original_hits(&payload)?;
    let fusion_weights = fusion_weights(&payload)?;
    let reproduced_hits = rerun_fusion(remeasured, &fusion_weights)?;
    let (reproduced, max_drift) =
        assert_within_tolerance(&original_hits, &reproduced_hits, REPRODUCE_TOLERANCE);
    Ok(ReproduceResult {
        reproduced,
        max_drift,
        original_hits,
        reproduced_hits,
    })
}

fn answer_payload(ledger_entries: &[LedgerEntry]) -> Result<Value> {
    let answer = ledger_entries
        .iter()
        .find(|entry| entry.kind == EntryKind::Answer)
        .ok_or_else(|| CalyxError::ledger_corrupt("reproduce context has no Answer entry"))?;
    serde_json::from_slice(&answer.payload)
        .map_err(|error| CalyxError::ledger_corrupt(format!("decode answer payload: {error}")))
}

fn original_hits(payload: &Value) -> Result<Vec<HitRef>> {
    let value = payload
        .get("original_hits")
        .ok_or_else(|| CalyxError::ledger_corrupt("missing original_hits"))?;
    serde_json::from_value(value.clone())
        .map_err(|error| CalyxError::ledger_corrupt(format!("decode original_hits: {error}")))
}

fn fusion_weights(payload: &Value) -> Result<FusionWeights> {
    let value = payload
        .get("fusion_weights")
        .ok_or_else(|| CalyxError::ledger_corrupt("missing fusion_weights"))?;
    serde_json::from_value(value.clone())
        .map_err(|error| CalyxError::ledger_corrupt(format!("decode fusion_weights: {error}")))
}

fn weight_map(weights: &[SlotWeight]) -> BTreeMap<PanelSlotId, f32> {
    weights
        .iter()
        .map(|weight| (weight.panel_slot, weight.weight))
        .collect()
}

fn slot_participates(slot: PanelSlotId, fusion_weights: &FusionWeights) -> bool {
    match fusion_weights.mode {
        FusionMode::SingleLens => fusion_weights.single_slot == Some(slot),
        FusionMode::Rrf | FusionMode::WeightedRrf => true,
    }
}

fn slot_weight(
    slot: PanelSlotId,
    fusion_weights: &FusionWeights,
    weights: &BTreeMap<PanelSlotId, f32>,
) -> f32 {
    match fusion_weights.mode {
        FusionMode::SingleLens | FusionMode::Rrf => 1.0,
        FusionMode::WeightedRrf => *weights.get(&slot).unwrap_or(&0.0),
    }
}

fn dense_scores(vector: &SlotVector) -> Result<&[f32]> {
    vector
        .as_dense()
        .ok_or_else(|| CalyxError::ledger_corrupt("fusion replay requires dense score vectors"))
}

fn ranked_indices(scores: &[f32]) -> Vec<usize> {
    let mut indices: Vec<_> = (0..scores.len()).collect();
    indices.sort_by(|left, right| {
        scores[*right]
            .total_cmp(&scores[*left])
            .then_with(|| left.cmp(right))
    });
    indices
}

pub fn reproduce_payload_bytes(
    answer_id: &QueryId,
    result: &ReproduceResult,
    ts: u64,
) -> Result<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({
        "type": REPRODUCE_PAYLOAD_TAG,
        "answer_id": hex(answer_id),
        "reproduced": result.reproduced,
        "max_drift": result.max_drift,
        "original_hits": result.original_hits,
        "reproduced_hits": result.reproduced_hits,
        "ts": ts,
    }))
    .map_err(|error| CalyxError::ledger_corrupt(format!("encode reproduce payload: {error}")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
