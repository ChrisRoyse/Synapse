//! Ledger-backed Lodestar provenance writers.

use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, CxId, LedgerRef, PanelSlotId};
use calyx_ledger::{
    ActorId, EntryKind, LedgerAppender, LedgerCfStore, PayloadBuilder, RedactionPolicy, SubjectId,
};
use calyx_paths::AssocGraph;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Kernel, KernelParams, LodestarError, Result, build_kernel_pipeline};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KernelBuildReceipt {
    pub kernel: Kernel,
    pub ledger_ref: LedgerRef,
}

/// Content-addressed inputs needed to independently re-derive a persisted
/// kernel answer. Raw query text is retained outside the ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KernelAnswerRecordContext {
    pub answer_id: Vec<u8>,
    pub query_input_sha256: [u8; 32],
    pub query_input_pointer: String,
    pub kernel_manifest_sha256: [u8; 32],
    pub embedding_slot: PanelSlotId,
    pub nearest_similarity: f32,
    pub admission_threshold: f32,
    pub resident_addr: String,
    pub anchor: Option<String>,
    pub max_hops: usize,
}

pub(crate) fn validate_kernel_answer_record_context(
    context: &KernelAnswerRecordContext,
) -> Result<()> {
    if context.answer_id.is_empty()
        || context.query_input_pointer.is_empty()
        || context.max_hops == 0
        || context.max_hops > 32
    {
        return Err(LodestarError::KernelInvalidParams {
            detail: "kernel answer record context needs a nonempty answer id/query pointer and max_hops in 1..=32"
                .to_string(),
        });
    }
    for (field, value) in [
        ("nearest_similarity", context.nearest_similarity),
        ("admission_threshold", context.admission_threshold),
    ] {
        if !value.is_finite() || !(-1.0..=1.0).contains(&value) {
            return Err(LodestarError::KernelInvalidParams {
                detail: format!("{field}={value} must be finite and in [-1,1]"),
            });
        }
    }
    Ok(())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn build_kernel_pipeline_with_ledger<S, C>(
    graph: &AssocGraph,
    anchors: &[CxId],
    params: &KernelParams,
    graph_seq: u64,
    ledger: &mut LedgerAppender<S, C>,
) -> Result<KernelBuildReceipt>
where
    S: LedgerCfStore,
    C: Clock,
{
    let kernel = build_kernel_pipeline(graph, anchors, params)?;
    let ledger_ref = append_kernel_build_entry(ledger, &kernel, graph_seq)?;
    Ok(KernelBuildReceipt { kernel, ledger_ref })
}

pub fn append_kernel_build_entry<S, C>(
    ledger: &mut LedgerAppender<S, C>,
    kernel: &Kernel,
    graph_seq: u64,
) -> Result<LedgerRef>
where
    S: LedgerCfStore,
    C: Clock,
{
    ledger
        .append(
            EntryKind::Kernel,
            SubjectId::Kernel(kernel.kernel_id.as_bytes().to_vec()),
            kernel_build_payload(kernel, graph_seq)?,
            ActorId::Service("calyx-lodestar".to_string()),
        )
        .map_err(Into::into)
}

pub fn append_answer_hop_entry<S, C>(
    ledger: &mut LedgerAppender<S, C>,
    query_cx: CxId,
    anchor_kernel_node: CxId,
    hop: AnswerHopEvidence,
) -> Result<LedgerRef>
where
    S: LedgerCfStore,
    C: Clock,
{
    ledger
        .append(
            EntryKind::Answer,
            SubjectId::Query(query_cx.as_bytes().to_vec()),
            answer_hop_payload(query_cx, anchor_kernel_node, hop)?,
            ActorId::Service("calyx-lodestar".to_string()),
        )
        .map_err(Into::into)
}

pub fn append_answer_complete_entry<S, C>(
    ledger: &mut LedgerAppender<S, C>,
    query_cx: CxId,
    anchor_kernel_node: CxId,
    kernel_id: CxId,
    hops: &[AnswerCompleteHopEvidence],
    total_score: f32,
) -> Result<LedgerRef>
where
    S: LedgerCfStore,
    C: Clock,
{
    ledger
        .append(
            EntryKind::Answer,
            SubjectId::Query(query_cx.as_bytes().to_vec()),
            complete_answer_payload(query_cx, anchor_kernel_node, kernel_id, hops, total_score)?,
            ActorId::Service("calyx-lodestar".to_string()),
        )
        .map_err(Into::into)
}

pub(crate) fn append_kernel_answer_hop_to_vault<C: Clock>(
    vault: &AsterVault<C>,
    context: &KernelAnswerRecordContext,
    query_cx: CxId,
    anchor_kernel_node: CxId,
    hop: AnswerHopEvidence,
) -> Result<LedgerRef> {
    vault
        .append_ledger_entry(
            EntryKind::Answer,
            SubjectId::Query(context.answer_id.clone()),
            kernel_answer_hop_payload(context, query_cx, anchor_kernel_node, hop)?,
            ActorId::Service("calyx-lodestar".to_string()),
        )
        .map_err(Into::into)
}

pub(crate) fn append_kernel_answer_complete_to_vault<C: Clock>(
    vault: &AsterVault<C>,
    context: &KernelAnswerRecordContext,
    record: KernelAnswerCompleteRecord<'_>,
) -> Result<LedgerRef> {
    vault
        .append_ledger_entry(
            EntryKind::Answer,
            SubjectId::Query(context.answer_id.clone()),
            kernel_answer_complete_payload(
                context,
                record.query_cx,
                record.anchor_kernel_node,
                record.kernel_id,
                record.hops,
                record.total_score,
                record.derivation_hash,
            )?,
            ActorId::Service("calyx-lodestar".to_string()),
        )
        .map_err(Into::into)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnswerHopEvidence {
    pub from: CxId,
    pub to: CxId,
    pub edge_weight: f32,
    pub hop_index: u32,
    pub hop_score: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnswerCompleteHopEvidence {
    pub from: CxId,
    pub to: CxId,
    pub edge_weight: f32,
    pub hop_index: u32,
    pub hop_score: f32,
    pub ledger_ref: LedgerRef,
}

pub(crate) struct KernelAnswerCompleteRecord<'a> {
    pub query_cx: CxId,
    pub anchor_kernel_node: CxId,
    pub kernel_id: CxId,
    pub hops: &'a [AnswerCompleteHopEvidence],
    pub total_score: f32,
    pub derivation_hash: [u8; 32],
}

pub fn kernel_members_hash(kernel: &Kernel) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"calyx-lodestar-kernel-members-v1");
    for member in &kernel.members {
        hasher.update(member.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn kernel_build_payload(kernel: &Kernel, graph_seq: u64) -> Result<Vec<u8>> {
    let mut payload = PayloadBuilder::default();
    payload
        .insert_str("kernel_id", kernel.kernel_id.to_string())
        .insert_str("members_hash", hex(&kernel_members_hash(kernel)))
        .insert_u64("graph_seq", graph_seq)
        .insert_value("mfvs_approx_factor", json!(kernel.recall.approx_factor))
        .insert_value(
            "mfvs_tau_star_estimate",
            json!(kernel.recall.tau_star_estimate),
        )
        .insert_value("mfvs_tau_star_exact", json!(kernel.recall.tau_star_exact))
        .insert_value("recall_ratio", json!(kernel.recall.ratio));
    let bytes = encode_payload(payload.value(), "kernel build")?;
    RedactionPolicy::check_payload(&bytes)?;
    Ok(bytes)
}

fn answer_hop_payload(
    query_cx: CxId,
    anchor_kernel_node: CxId,
    hop: AnswerHopEvidence,
) -> Result<Vec<u8>> {
    let mut payload = PayloadBuilder::default();
    payload
        .insert_str("query_id", query_cx.to_string())
        .insert_str("anchor_kernel_node_id", anchor_kernel_node.to_string())
        .insert_str("from_id", hop.from.to_string())
        .insert_str("to_id", hop.to.to_string())
        .insert_u64("hop_index", u64::from(hop.hop_index))
        .insert_value("edge_weight", json!(hop.edge_weight))
        .insert_value("hop_score", json!(hop.hop_score));
    let bytes = encode_payload(payload.value(), "answer hop")?;
    RedactionPolicy::check_payload(&bytes)?;
    Ok(bytes)
}

fn kernel_answer_hop_payload(
    context: &KernelAnswerRecordContext,
    query_cx: CxId,
    anchor_kernel_node: CxId,
    hop: AnswerHopEvidence,
) -> Result<Vec<u8>> {
    let mut payload = PayloadBuilder::default();
    payload
        .insert_str("type", "kernel_answer_hop_v1")
        .insert_str("answer_id", hex(&context.answer_id))
        .insert_str("query_id", query_cx.to_string())
        .insert_str("anchor_kernel_node_id", anchor_kernel_node.to_string())
        .insert_str("from_id", hop.from.to_string())
        .insert_str("to_id", hop.to.to_string())
        .insert_u64("hop_index", u64::from(hop.hop_index))
        .insert_value("edge_weight", json!(hop.edge_weight))
        .insert_value("hop_score", json!(hop.hop_score));
    let bytes = encode_payload(payload.value(), "kernel answer hop")?;
    RedactionPolicy::check_payload(&bytes)?;
    Ok(bytes)
}

fn complete_answer_payload(
    query_cx: CxId,
    anchor_kernel_node: CxId,
    kernel_id: CxId,
    hops: &[AnswerCompleteHopEvidence],
    total_score: f32,
) -> Result<Vec<u8>> {
    let path = hops
        .iter()
        .map(|hop| {
            json!({
                "from_id": hop.from.to_string(),
                "cx_id": hop.to.to_string(),
                "to_id": hop.to.to_string(),
                "hop": hop.hop_index,
                "hop_index": hop.hop_index,
                "score": hop.hop_score,
                "hop_score": hop.hop_score,
                "edge_weight": hop.edge_weight,
                "ledger_ref": {
                    "seq": hop.ledger_ref.seq,
                    "hash": hex(&hop.ledger_ref.hash),
                },
            })
        })
        .collect::<Vec<_>>();
    let mut payload = PayloadBuilder::default();
    payload
        .insert_value("complete", json!(true))
        .insert_u64("expected_hops", hops.len() as u64)
        .insert_str("query_id", query_cx.to_string())
        .insert_str("anchor_kernel_node_id", anchor_kernel_node.to_string())
        .insert_str("kernel_id", kernel_id.to_string())
        .insert_value("total_score", json!(total_score))
        .insert_value("path", json!(path));
    let bytes = encode_payload(payload.value(), "complete answer")?;
    RedactionPolicy::check_payload(&bytes)?;
    Ok(bytes)
}

fn kernel_answer_complete_payload(
    context: &KernelAnswerRecordContext,
    query_cx: CxId,
    anchor_kernel_node: CxId,
    kernel_id: CxId,
    hops: &[AnswerCompleteHopEvidence],
    total_score: f32,
    derivation_hash: [u8; 32],
) -> Result<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(&complete_answer_payload(
        query_cx,
        anchor_kernel_node,
        kernel_id,
        hops,
        total_score,
    )?)
    .map_err(|error| LodestarError::KernelProvenancePayloadCodec {
        detail: format!("decode generated complete answer payload: {error}"),
    })?;
    let object =
        value
            .as_object_mut()
            .ok_or_else(|| LodestarError::KernelProvenancePayloadCodec {
                detail: "generated complete answer payload is not a JSON object".to_string(),
            })?;
    object.insert("type".to_string(), json!("kernel_answer_v1"));
    object.insert("answer_id".to_string(), json!(hex(&context.answer_id)));
    object.insert(
        "query_input_sha256".to_string(),
        json!(hex(&context.query_input_sha256)),
    );
    object.insert(
        "query_input_pointer".to_string(),
        json!(context.query_input_pointer),
    );
    object.insert(
        "kernel_manifest_sha256".to_string(),
        json!(hex(&context.kernel_manifest_sha256)),
    );
    object.insert(
        "embedding_slot".to_string(),
        json!({
            "panel_version": context.embedding_slot.panel_version(),
            "slot_id": context.embedding_slot.slot_id().get(),
        }),
    );
    object.insert(
        "nearest_similarity".to_string(),
        json!(context.nearest_similarity),
    );
    object.insert(
        "admission_threshold".to_string(),
        json!(context.admission_threshold),
    );
    object.insert("resident_addr".to_string(), json!(context.resident_addr));
    object.insert("anchor".to_string(), json!(context.anchor));
    object.insert("max_hops".to_string(), json!(context.max_hops));
    object.insert("derivation_hash".to_string(), json!(hex(&derivation_hash)));
    let bytes = encode_payload(&value, "kernel complete answer")?;
    RedactionPolicy::check_payload(&bytes)?;
    Ok(bytes)
}

fn encode_payload(value: &serde_json::Value, label: &str) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| LodestarError::KernelProvenancePayloadCodec {
        detail: format!("encode {label} payload: {error}"),
    })
}
