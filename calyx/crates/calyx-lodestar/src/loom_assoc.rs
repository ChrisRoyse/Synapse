use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{CxId, PanelSlotId};
use calyx_loom::{CrossTermKind, CrossTermValue, LoomStore};
use calyx_mincut::{AgreementEdge, build_assoc_graph};
use calyx_paths::AssocGraph;
use serde::{Deserialize, Serialize};

use crate::{LodestarError, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LoomSlotNode {
    pub xterm_cx: CxId,
    pub slot: PanelSlotId,
    pub node: CxId,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoomDirectionalConfidence {
    pub xterm_cx: CxId,
    pub src_slot: PanelSlotId,
    pub dst_slot: PanelSlotId,
    pub confidence: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoomAssocGraphInput {
    pub agreements: Vec<AgreementEdge>,
    pub provenance: Vec<LoomAssocEdgeProvenance>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoomAssocEdgeProvenance {
    pub xterm_cx: CxId,
    pub src_slot: PanelSlotId,
    pub dst_slot: PanelSlotId,
    pub src_cx: CxId,
    pub dst_cx: CxId,
    pub raw_agreement: f32,
    pub agreement: f32,
    pub directional_confidence: f32,
    pub edge_weight: f32,
}

pub fn loom_assoc_graph_input(
    store: &LoomStore,
    slot_nodes: &[LoomSlotNode],
    directional_confidences: &[LoomDirectionalConfidence],
) -> Result<LoomAssocGraphInput> {
    let node_map = slot_node_map(slot_nodes);
    let agreements = agreement_rows(store)?;
    let confidence_map = directional_confidence_map(directional_confidences)?;
    let mut seen_pairs = BTreeSet::new();
    let mut out = Vec::new();
    let mut provenance = Vec::new();

    for (key, confidence) in confidence_map {
        if key.src_slot.panel_version() != key.dst_slot.panel_version() {
            return Err(LodestarError::KernelLoomAgreementInvalid {
                detail: format!(
                    "directional confidence spans panels {} and {}",
                    key.src_slot, key.dst_slot
                ),
            });
        }
        let (a, b) = canonical_panel_pair(key.src_slot, key.dst_slot);
        let pair_key = LoomPairKey {
            xterm_cx: key.xterm_cx,
            a,
            b,
        };
        let Some(raw_agreement) = agreements.get(&pair_key).copied() else {
            return Err(LodestarError::KernelLoomAgreementMissing {
                xterm_cx: key.xterm_cx,
                a,
                b,
            });
        };
        let src_cx = mapped_node(&node_map, key.xterm_cx, key.src_slot)?;
        let dst_cx = mapped_node(&node_map, key.xterm_cx, key.dst_slot)?;
        let agreement = agreement_weight(raw_agreement)?;
        let edge_weight = agreement * confidence;

        out.push(AgreementEdge {
            src: src_cx,
            dst: dst_cx,
            agreement,
            directional_confidence: confidence,
        });
        provenance.push(LoomAssocEdgeProvenance {
            xterm_cx: key.xterm_cx,
            src_slot: key.src_slot,
            dst_slot: key.dst_slot,
            src_cx,
            dst_cx,
            raw_agreement,
            agreement,
            directional_confidence: confidence,
            edge_weight,
        });
        seen_pairs.insert(pair_key);
    }

    for pair in agreements.keys() {
        if !seen_pairs.contains(pair) {
            return Err(LodestarError::KernelLoomDirectionalConfidenceMissing {
                xterm_cx: pair.xterm_cx,
                a: pair.a,
                b: pair.b,
            });
        }
    }

    Ok(LoomAssocGraphInput {
        agreements: out,
        provenance,
    })
}

pub fn build_assoc_graph_from_loom(
    store: &LoomStore,
    slot_nodes: &[LoomSlotNode],
    directional_confidences: &[LoomDirectionalConfidence],
) -> Result<(AssocGraph, Vec<LoomAssocEdgeProvenance>)> {
    let input = loom_assoc_graph_input(store, slot_nodes, directional_confidences)?;
    let graph = build_assoc_graph(&input.agreements, &[], &[])?;
    Ok((graph, input.provenance))
}

fn slot_node_map(slot_nodes: &[LoomSlotNode]) -> BTreeMap<(CxId, PanelSlotId), CxId> {
    slot_nodes
        .iter()
        .map(|node| ((node.xterm_cx, node.slot), node.node))
        .collect()
}

fn mapped_node(
    node_map: &BTreeMap<(CxId, PanelSlotId), CxId>,
    xterm_cx: CxId,
    slot: PanelSlotId,
) -> Result<CxId> {
    node_map
        .get(&(xterm_cx, slot))
        .copied()
        .ok_or(LodestarError::KernelLoomSlotMappingMissing { xterm_cx, slot })
}

fn agreement_rows(store: &LoomStore) -> Result<BTreeMap<LoomPairKey, f32>> {
    let mut rows = BTreeMap::new();
    for row in store.xterm_rows() {
        if row.key.kind != CrossTermKind::Agreement {
            continue;
        }
        let CrossTermValue::Scalar(value) = row.value else {
            return Err(LodestarError::KernelLoomAgreementInvalid {
                detail: format!("agreement xterm for {} is not scalar", row.key.cx_id),
            });
        };
        agreement_weight(value)?;
        rows.insert(
            LoomPairKey {
                xterm_cx: row.key.cx_id,
                a: row.key.a,
                b: row.key.b,
            },
            value,
        );
    }
    Ok(rows)
}

fn directional_confidence_map(
    confidences: &[LoomDirectionalConfidence],
) -> Result<BTreeMap<LoomConfidenceKey, f32>> {
    let mut out = BTreeMap::new();
    for confidence in confidences {
        if !(confidence.confidence.is_finite() && (0.0..=1.0).contains(&confidence.confidence)) {
            return Err(LodestarError::KernelLoomAgreementInvalid {
                detail: format!(
                    "directional_confidence={} must be finite and in [0,1]",
                    confidence.confidence
                ),
            });
        }
        let key = LoomConfidenceKey {
            xterm_cx: confidence.xterm_cx,
            src_slot: confidence.src_slot,
            dst_slot: confidence.dst_slot,
        };
        if out.insert(key, confidence.confidence).is_some() {
            return Err(LodestarError::KernelLoomAgreementInvalid {
                detail: format!(
                    "duplicate directional confidence for {}/{}/{}",
                    key.xterm_cx, key.src_slot, key.dst_slot
                ),
            });
        }
    }
    Ok(out)
}

fn agreement_weight(raw: f32) -> Result<f32> {
    if raw.is_finite() {
        Ok(raw.clamp(0.0, 1.0))
    } else {
        Err(LodestarError::KernelLoomAgreementInvalid {
            detail: format!("raw agreement {raw} must be finite"),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LoomPairKey {
    xterm_cx: CxId,
    a: PanelSlotId,
    b: PanelSlotId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct LoomConfidenceKey {
    xterm_cx: CxId,
    src_slot: PanelSlotId,
    dst_slot: PanelSlotId,
}

fn canonical_panel_pair(a: PanelSlotId, b: PanelSlotId) -> (PanelSlotId, PanelSlotId) {
    if a <= b { (a, b) } else { (b, a) }
}
