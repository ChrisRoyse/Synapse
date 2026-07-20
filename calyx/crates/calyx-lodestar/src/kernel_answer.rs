use calyx_aster::ledger_view::AsterLedgerCfStore;
use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, CxId, LedgerRef};
use calyx_ledger::{EntryKind, LedgerAppender, LedgerCfStore, decode};
use calyx_paths::{AssocGraph, attenuate, reach};
use serde::{Deserialize, Serialize};

use crate::provenance::{
    AnswerCompleteHopEvidence, AnswerHopEvidence, KernelAnswerCompleteRecord,
    append_answer_complete_entry, append_answer_hop_entry, append_kernel_answer_complete_to_vault,
    append_kernel_answer_hop_to_vault, hex, validate_kernel_answer_record_context,
};
use crate::{KernelAnswerRecordContext, KernelIndex, LodestarError, Result, kernel_search};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerDerivation {
    pub query_cx: CxId,
    pub anchor_kernel_node: CxId,
    pub kernel_id: CxId,
    pub hops: Vec<AnswerDerivationHop>,
    pub total_score: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerDerivationHop {
    pub from: CxId,
    pub to: CxId,
    pub edge_weight: f32,
    pub hop_index: u32,
    pub hop_score: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerPath {
    pub query_cx: CxId,
    pub anchor_kernel_node: CxId,
    pub hops: Vec<AnswerHop>,
    pub total_score: f32,
    pub provenance: Vec<LedgerRef>,
}

pub struct AsterKernelAnswerRequest<'a, C: Clock> {
    pub kernel_index: &'a KernelIndex,
    pub graph: &'a AssocGraph,
    pub query_cx: CxId,
    pub query_vec: &'a [f32],
    pub anchored_kernel_nodes: &'a [CxId],
    pub max_hops: usize,
    pub context: &'a KernelAnswerRecordContext,
    pub vault: &'a AsterVault<C>,
    pub vault_dir: &'a std::path::Path,
}

impl AnswerPath {
    pub fn checked(
        query_cx: CxId,
        anchor_kernel_node: CxId,
        hops: Vec<AnswerHop>,
        total_score: f32,
    ) -> Result<Self> {
        validate_score(total_score, "total_score")?;
        let provenance = hops.iter().map(|hop| hop.ledger_ref.clone()).collect();
        Ok(Self {
            query_cx,
            anchor_kernel_node,
            hops,
            total_score,
            provenance,
        })
    }

    fn checked_with_complete_ref(
        query_cx: CxId,
        anchor_kernel_node: CxId,
        hops: Vec<AnswerHop>,
        total_score: f32,
        complete_ref: LedgerRef,
    ) -> Result<Self> {
        let mut answer = Self::checked(query_cx, anchor_kernel_node, hops, total_score)?;
        answer.provenance.push(complete_ref);
        Ok(answer)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerHop {
    pub from: CxId,
    pub to: CxId,
    pub edge_weight: f32,
    pub hop_index: u32,
    pub hop_score: f32,
    pub ledger_ref: LedgerRef,
}

pub fn kernel_answer(
    kernel_index: &KernelIndex,
    graph: &AssocGraph,
    query_cx: CxId,
    query_vec: &[f32],
    anchored_kernel_nodes: &[CxId],
    max_hops: usize,
) -> Result<AnswerPath> {
    let derivation = derive_kernel_answer(
        kernel_index,
        graph,
        query_cx,
        query_vec,
        anchored_kernel_nodes,
        max_hops,
    )?;
    Err(LodestarError::KernelAnswerLedgerRequired {
        detail: format!(
            "kernel_answer found a {}-hop path from anchor {anchor} to query {query_cx}, but answer provenance requires kernel_answer_with_ledger",
            derivation.hops.len(),
            anchor = derivation.anchor_kernel_node,
        ),
    })
}

pub fn derive_kernel_answer(
    kernel_index: &KernelIndex,
    graph: &AssocGraph,
    query_cx: CxId,
    query_vec: &[f32],
    anchored_kernel_nodes: &[CxId],
    max_hops: usize,
) -> Result<AnswerDerivation> {
    let (anchor, path) = nearest_answerable_anchored_path(
        kernel_index,
        graph,
        query_cx,
        query_vec,
        anchored_kernel_nodes,
        max_hops,
    )?;
    let hops = derivation_hops(graph, &path)?;
    let total_score = if hops.is_empty() {
        1.0
    } else {
        hops.iter().map(|hop| hop.hop_score).sum()
    };
    validate_score(total_score, "total_score")?;
    Ok(AnswerDerivation {
        query_cx,
        anchor_kernel_node: anchor,
        kernel_id: kernel_index.kernel_id,
        hops,
        total_score,
    })
}

pub fn kernel_answer_derivation_hash(
    derivation: &AnswerDerivation,
    context: &KernelAnswerRecordContext,
) -> Result<[u8; 32]> {
    validate_kernel_answer_record_context(context)?;
    let bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 2,
        "answer_id": hex(&context.answer_id),
        "query_input_sha256": hex(&context.query_input_sha256),
        "kernel_manifest_sha256": hex(&context.kernel_manifest_sha256),
        "embedding_slot": {
            "panel_version": context.embedding_slot.panel_version(),
            "slot_id": context.embedding_slot.slot_id().get(),
        },
        "nearest_similarity": context.nearest_similarity,
        "admission_threshold": context.admission_threshold,
        "anchor": context.anchor,
        "max_hops": context.max_hops,
        "derivation": derivation,
    }))
    .map_err(|error| LodestarError::KernelArtifactCodec {
        detail: format!("encode kernel answer derivation: {error}"),
    })?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

pub fn kernel_answer_with_ledger<S, C>(
    kernel_index: &KernelIndex,
    graph: &AssocGraph,
    query_cx: CxId,
    query_vec: &[f32],
    anchored_kernel_nodes: &[CxId],
    max_hops: usize,
    ledger: &mut LedgerAppender<S, C>,
) -> Result<AnswerPath>
where
    S: LedgerCfStore,
    C: Clock,
{
    let derivation = derive_kernel_answer(
        kernel_index,
        graph,
        query_cx,
        query_vec,
        anchored_kernel_nodes,
        max_hops,
    )?;
    let answer = if derivation.hops.is_empty() {
        let complete_ref = append_answer_complete_entry(
            ledger,
            query_cx,
            derivation.anchor_kernel_node,
            kernel_index.kernel_id,
            &[],
            1.0,
        )?;
        AnswerPath::checked_with_complete_ref(
            query_cx,
            derivation.anchor_kernel_node,
            Vec::new(),
            1.0,
            complete_ref,
        )
    } else {
        let hops = answer_hops_with(
            &derivation,
            |from, to, hop_index, edge_weight, hop_score| {
                append_answer_hop_entry(
                    ledger,
                    query_cx,
                    derivation.anchor_kernel_node,
                    AnswerHopEvidence {
                        from,
                        to,
                        edge_weight,
                        hop_index,
                        hop_score,
                    },
                )
            },
        )?;
        let total_score = derivation.total_score;
        let complete_hops = hops
            .iter()
            .map(|hop| AnswerCompleteHopEvidence {
                from: hop.from,
                to: hop.to,
                edge_weight: hop.edge_weight,
                hop_index: hop.hop_index,
                hop_score: hop.hop_score,
                ledger_ref: hop.ledger_ref.clone(),
            })
            .collect::<Vec<_>>();
        let complete_ref = append_answer_complete_entry(
            ledger,
            query_cx,
            derivation.anchor_kernel_node,
            kernel_index.kernel_id,
            &complete_hops,
            total_score,
        )?;
        AnswerPath::checked_with_complete_ref(
            query_cx,
            derivation.anchor_kernel_node,
            hops,
            total_score,
            complete_ref,
        )
    }?;
    verify_answer_ledger_refs(ledger.store(), &answer.provenance)?;
    Ok(answer)
}

pub fn kernel_answer_with_aster_ledger<C: Clock>(
    request: AsterKernelAnswerRequest<'_, C>,
) -> Result<AnswerPath> {
    let AsterKernelAnswerRequest {
        kernel_index,
        graph,
        query_cx,
        query_vec,
        anchored_kernel_nodes,
        max_hops,
        context,
        vault,
        vault_dir,
    } = request;
    validate_kernel_answer_record_context(context)?;
    let derivation = derive_kernel_answer(
        kernel_index,
        graph,
        query_cx,
        query_vec,
        anchored_kernel_nodes,
        max_hops,
    )?;
    let hops = answer_hops_with(
        &derivation,
        |from, to, hop_index, edge_weight, hop_score| {
            append_kernel_answer_hop_to_vault(
                vault,
                context,
                query_cx,
                derivation.anchor_kernel_node,
                AnswerHopEvidence {
                    from,
                    to,
                    edge_weight,
                    hop_index,
                    hop_score,
                },
            )
        },
    )?;
    let complete_hops = hops
        .iter()
        .map(|hop| AnswerCompleteHopEvidence {
            from: hop.from,
            to: hop.to,
            edge_weight: hop.edge_weight,
            hop_index: hop.hop_index,
            hop_score: hop.hop_score,
            ledger_ref: hop.ledger_ref.clone(),
        })
        .collect::<Vec<_>>();
    let derivation_hash = kernel_answer_derivation_hash(&derivation, context)?;
    let complete_ref = append_kernel_answer_complete_to_vault(
        vault,
        context,
        KernelAnswerCompleteRecord {
            query_cx,
            anchor_kernel_node: derivation.anchor_kernel_node,
            kernel_id: kernel_index.kernel_id,
            hops: &complete_hops,
            total_score: derivation.total_score,
            derivation_hash,
        },
    )?;
    let answer = AnswerPath::checked_with_complete_ref(
        query_cx,
        derivation.anchor_kernel_node,
        hops,
        derivation.total_score,
        complete_ref,
    )?;
    let physical = AsterLedgerCfStore::open(vault_dir)?;
    verify_answer_ledger_refs(&physical, &answer.provenance)?;
    Ok(answer)
}

fn nearest_answerable_anchored_path(
    index: &KernelIndex,
    graph: &AssocGraph,
    query_cx: CxId,
    query_vec: &[f32],
    anchored_nodes: &[CxId],
    max_hops: usize,
) -> Result<(CxId, Vec<CxId>)> {
    if anchored_nodes.is_empty() {
        return Err(LodestarError::KernelNoAnchoredNode);
    }
    let candidates = kernel_search(index, query_vec, index.rows().len())?;
    let mut saw_anchored_candidate = false;
    let mut first_path_error = None;
    for anchor in candidates
        .into_iter()
        .map(|(cx_id, _)| cx_id)
        .filter(|cx_id| anchored_nodes.contains(cx_id))
    {
        saw_anchored_candidate = true;
        if graph.node_index(anchor).is_none() {
            continue;
        }
        if query_cx == anchor {
            return Ok((anchor, vec![anchor]));
        }
        match reach(graph, anchor, query_cx, max_hops) {
            Ok(Some(path)) => return Ok((anchor, path)),
            Ok(None) => {
                first_path_error.get_or_insert(LodestarError::KernelAnswerNoPath {
                    from: anchor,
                    to: query_cx,
                });
            }
            Err(err) => {
                let error = LodestarError::from(err);
                if error.code() != "CALYX_PATHS_MAX_HOPS" {
                    return Err(error);
                }
                first_path_error.get_or_insert(error);
            }
        }
    }
    if !saw_anchored_candidate {
        return Err(LodestarError::KernelNoAnchoredNode);
    }
    Err(first_path_error.unwrap_or(LodestarError::KernelNoAnchoredNode))
}

fn derivation_hops(graph: &AssocGraph, path: &[CxId]) -> Result<Vec<AnswerDerivationHop>> {
    path.windows(2)
        .enumerate()
        .map(|(idx, pair)| {
            let from = pair[0];
            let to = pair[1];
            let edge_weight = edge_weight(graph, from, to)?;
            let hop_index = idx as u32;
            let hop_score = attenuate(edge_weight, hop_index);
            validate_score(hop_score, "hop_score")?;
            Ok(AnswerDerivationHop {
                from,
                to,
                edge_weight,
                hop_index,
                hop_score,
            })
        })
        .collect()
}

fn answer_hops_with<F>(derivation: &AnswerDerivation, mut ledger_ref: F) -> Result<Vec<AnswerHop>>
where
    F: FnMut(CxId, CxId, u32, f32, f32) -> Result<LedgerRef>,
{
    derivation
        .hops
        .iter()
        .map(|hop| {
            let ledger_ref = ledger_ref(
                hop.from,
                hop.to,
                hop.hop_index,
                hop.edge_weight,
                hop.hop_score,
            )?;
            Ok(AnswerHop {
                from: hop.from,
                to: hop.to,
                edge_weight: hop.edge_weight,
                hop_index: hop.hop_index,
                hop_score: hop.hop_score,
                ledger_ref,
            })
        })
        .collect()
}

fn edge_weight(graph: &AssocGraph, from: CxId, to: CxId) -> Result<f32> {
    let from_idx = graph.require_node_index(from)?;
    let to_idx = graph.require_node_index(to)?;
    graph
        .out_edges_by_index(from_idx)
        .iter()
        .find_map(|edge| (edge.dst == to_idx).then_some(edge.weight))
        .ok_or(LodestarError::KernelAnswerNoPath { from, to })
}

fn validate_score(score: f32, field: &str) -> Result<()> {
    if score.is_finite() && score >= 0.0 {
        Ok(())
    } else {
        Err(LodestarError::KernelScoreInvalid {
            detail: format!("{field}={score} must be finite and non-negative"),
        })
    }
}

fn verify_answer_ledger_refs<S: LedgerCfStore>(store: &S, refs: &[LedgerRef]) -> Result<()> {
    for reference in refs {
        let row = store.read_seq(reference.seq)?.ok_or_else(|| {
            LodestarError::KernelAnswerLedgerMismatch {
                detail: format!("answer ledger seq {} is absent", reference.seq),
            }
        })?;
        let entry = decode(&row.bytes)?;
        if entry.seq != reference.seq || row.seq != reference.seq {
            return Err(LodestarError::KernelAnswerLedgerMismatch {
                detail: format!(
                    "answer ledger ref seq {} read row key seq {} encoded seq {}",
                    reference.seq, row.seq, entry.seq
                ),
            });
        }
        if entry.kind != EntryKind::Answer {
            return Err(LodestarError::KernelAnswerLedgerMismatch {
                detail: format!(
                    "answer ledger seq {} has kind {}, expected answer",
                    reference.seq,
                    entry.kind.as_str()
                ),
            });
        }
        if entry.entry_hash != reference.hash {
            return Err(LodestarError::KernelAnswerLedgerMismatch {
                detail: format!(
                    "answer ledger seq {} hash does not match referenced entry hash",
                    reference.seq
                ),
            });
        }
    }
    Ok(())
}
