//! Build a real Lodestar kernel AND measure its kernel-only recall directly from
//! a live Aster vault — the production Vault→embeddings recall bridge (#1900).
//!
//! Embeddings are read straight from each constellation's content-slot dense
//! vector (the source of truth — no mock, no fabricated recall). Associations
//! are derived as the embedding k-NN graph (concepts the panel measures as
//! close). The kernel is selected by [`build_kernel_pipeline`] and its recall is
//! MEASURED by [`measure_kernel_recall`] against the full corpus index. Fails loud
//! on a too-small / unanchored / unembedded vault.

use std::collections::BTreeMap;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, CxId, PanelSlotId, SlotVector, VaultStore};
use calyx_paths::AssocGraph;
use calyx_sextant::{HnswIndex, SextantIndex};

use crate::error::{LodestarError, Result};
use crate::{
    AnnIndex, InMemoryAnnIndex, InMemoryCorpus, Kernel, KernelParams, RecallEvalParams,
    RecallQuery, RecallReport, build_kernel_index, build_kernel_pipeline, measure_kernel_recall,
};

/// A real kernel plus its MEASURED kernel-only recall, both computed from the
/// live vault corpus.
pub struct MeasuredVaultKernel {
    pub kernel: Kernel,
    pub recall: RecallReport,
    /// Number of embedded concepts in the corpus the kernel was measured against.
    pub corpus_size: usize,
    /// Number of concepts visible in the vault Base CF at the measurement snapshot.
    pub vault_corpus_size: usize,
}

const VAULT_GRAPH_EXACT_MAX_ROWS: usize = 4_096;
const VAULT_GRAPH_INDEX_SEED: u64 = 0xCA1A_4A77_10DE_57A9;

/// Build the doc-corpus kernel for `vault` and measure its kernel-only recall.
///
/// `content_slot` is the dense semantic lens slot read per concept. `knn` /
/// `edge_cos_threshold` shape the embedding-proximity association graph that
/// drives kernel-member selection. `recall_params.min_recall_ratio` is the gate
/// (0.95 for the website). Errors (never silent): a vault with <2 embedded
/// concepts, no anchored concepts, or a concept missing the content-slot vector.
pub fn measured_kernel_from_vault<C: Clock>(
    vault: &AsterVault<C>,
    content_slot: PanelSlotId,
    kernel_params: &KernelParams,
    recall_params: &RecallEvalParams,
    knn: usize,
    edge_cos_threshold: f32,
) -> Result<MeasuredVaultKernel> {
    let inputs =
        build_vault_kernel_inputs(vault, content_slot, kernel_params, knn, edge_cos_threshold)?;
    let kernel_index = build_kernel_index(&inputs.kernel, &inputs.embeddings)?;
    let recall = measure_kernel_recall(&kernel_index, &inputs.full, &inputs.corpus, recall_params)?;
    Ok(MeasuredVaultKernel {
        kernel: inputs.kernel,
        recall,
        corpus_size: inputs.corpus_size,
        vault_corpus_size: inputs.vault_corpus_size,
    })
}

/// Build the measured kernel AND each member's **leave-one-out recall
/// contribution** (#1901).
///
/// `contributions[i] = baseline_kernel_only_recall − recall_without(member_i)`:
/// the drop in MEASURED kernel-only recall when that member is removed from the
/// kernel (the retrieval corpus is held fixed — only the kernel index shrinks).
/// A large positive value means the member carries recall the others do not; a
/// value near zero means it is redundant; a negative value means it was hurting.
/// The corpus/full index are built once and reused, so the cost is `n` extra
/// recall tests over the same corpus (the caller caches the result — #1898).
/// The sole-member case reports the full baseline (removing it leaves no kernel
/// to evaluate). NOT fabricated — every value is a real `measure_kernel_recall`.
pub fn measured_kernel_with_contributions_from_vault<C: Clock>(
    vault: &AsterVault<C>,
    content_slot: PanelSlotId,
    kernel_params: &KernelParams,
    recall_params: &RecallEvalParams,
    knn: usize,
    edge_cos_threshold: f32,
) -> Result<(MeasuredVaultKernel, Vec<(CxId, f32)>)> {
    let inputs =
        build_vault_kernel_inputs(vault, content_slot, kernel_params, knn, edge_cos_threshold)?;
    measured_kernel_with_contributions_from_inputs(inputs, recall_params)
}

fn measured_kernel_with_contributions_from_inputs(
    inputs: VaultKernelInputs,
    recall_params: &RecallEvalParams,
) -> Result<(MeasuredVaultKernel, Vec<(CxId, f32)>)> {
    let kernel_index = build_kernel_index(&inputs.kernel, &inputs.embeddings)?;
    let recall = measure_kernel_recall(&kernel_index, &inputs.full, &inputs.corpus, recall_params)?;
    let baseline = recall.kernel_only;

    let mut contributions: Vec<(CxId, f32)> = Vec::with_capacity(inputs.kernel.members.len());
    for member in &inputs.kernel.members {
        let drop = if inputs.kernel.members.len() == 1 {
            // Removing the only member leaves nothing to recall-test; the member
            // accounts for the whole baseline by definition.
            baseline
        } else {
            let mut leave_one_out = inputs.kernel.clone();
            leave_one_out.members.retain(|m| m != member);
            let loo_index = build_kernel_index(&leave_one_out, &inputs.embeddings)?;
            let loo_recall =
                measure_kernel_recall(&loo_index, &inputs.full, &inputs.corpus, recall_params)?;
            baseline - loo_recall.kernel_only
        };
        contributions.push((*member, drop));
    }

    Ok((
        MeasuredVaultKernel {
            kernel: inputs.kernel,
            recall,
            corpus_size: inputs.corpus_size,
            vault_corpus_size: inputs.vault_corpus_size,
        },
        contributions,
    ))
}

/// The intermediate inputs shared by [`measured_kernel_from_vault`] and
/// [`measured_kernel_with_contributions_from_vault`]: the selected kernel, the
/// per-concept embeddings, and the full-corpus retrieval index/corpus the
/// kernel's recall is measured against.
struct VaultKernelInputs {
    kernel: Kernel,
    embeddings: BTreeMap<CxId, Vec<f32>>,
    full: InMemoryAnnIndex,
    corpus: InMemoryCorpus,
    corpus_size: usize,
    vault_corpus_size: usize,
}

/// Scan the vault's content-slot embeddings, build the embedding k-NN
/// association graph, select the kernel, and build the full-corpus index — the
/// setup common to every measured-kernel call. Fails loud (never silent) on a
/// too-small / unanchored / unembedded vault.
fn build_vault_kernel_inputs<C: Clock>(
    vault: &AsterVault<C>,
    content_slot: PanelSlotId,
    kernel_params: &KernelParams,
    knn: usize,
    edge_cos_threshold: f32,
) -> Result<VaultKernelInputs> {
    if kernel_params.panel_version != content_slot.panel_version() {
        return Err(LodestarError::KernelInvalidParams {
            detail: format!(
                "kernel panel {} does not match content {content_slot}",
                kernel_params.panel_version
            ),
        });
    }
    let snapshot = vault.snapshot();
    let mut rows: Vec<RecallQuery> = Vec::new();
    let mut anchors: Vec<CxId> = Vec::new();
    let mut vault_corpus_size = 0usize;
    for (key, _) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        let bytes: [u8; 16] =
            key.as_slice()
                .try_into()
                .map_err(|_| LodestarError::KernelInvalidParams {
                    detail: format!("base CF key has {} bytes, expected 16", key.len()),
                })?;
        let cx_id = CxId::from_bytes(bytes);
        let cx = vault.get(cx_id, snapshot)?;
        if cx.panel_version != content_slot.panel_version() {
            continue;
        }
        vault_corpus_size += 1;
        let Some(dense) = cx
            .slots
            .get(&content_slot.slot_id())
            .and_then(|vector| vector.as_dense())
        else {
            return Err(LodestarError::KernelInvalidParams {
                detail: format!(
                    "constellation {cx_id} has no dense vector in {content_slot}; \
                     the kernel needs a per-concept embedding"
                ),
            });
        };
        rows.push(RecallQuery {
            cx_id,
            vector: dense.to_vec(),
        });
        if !cx.anchors.is_empty() {
            anchors.push(cx_id);
        }
    }
    if rows.len() < 2 {
        return Err(LodestarError::KernelInvalidParams {
            detail: format!(
                "vault has {} embedded concept(s) in slot {content_slot}; need >=2 for a kernel",
                rows.len()
            ),
        });
    }
    if anchors.is_empty() {
        return Err(LodestarError::KernelInvalidParams {
            detail: "vault has no anchored concepts; anchor at least one before building a kernel"
                .to_string(),
        });
    }

    let full = InMemoryAnnIndex::new(rows.clone())?;
    let graph_ann = if rows.len() > VAULT_GRAPH_EXACT_MAX_ROWS {
        Some(build_vault_graph_ann_index(&rows, content_slot)?)
    } else {
        None
    };
    let graph_index: &dyn AnnIndex = match graph_ann.as_ref() {
        Some(index) => index,
        None => &full,
    };

    // Embedding k-NN association graph: an edge for each candidate the full
    // corpus index measures as close (cosine >= threshold), up to `knn`
    // neighbours per node.
    let mut builder = AssocGraph::builder();
    for row in &rows {
        builder.add_node(row.cx_id, 1.0)?;
    }
    if knn > 0 {
        let candidate_count = knn.saturating_add(1).min(rows.len());
        for src in &rows {
            let neighbours = graph_index.search(&src.vector, candidate_count)?;
            for (dst, cosine) in neighbours
                .into_iter()
                .filter(|(dst, _)| *dst != src.cx_id)
                .filter(|(_, cosine)| *cosine >= edge_cos_threshold)
                .take(knn)
            {
                builder.add_edge(src.cx_id, dst, cosine)?;
            }
        }
    }
    let graph = builder.build();

    let kernel = build_kernel_pipeline(&graph, &anchors, kernel_params)?;
    // Injecting fallback members fabricates recall and invalidates the kernel identity.
    if kernel.members.is_empty() {
        return Err(LodestarError::KernelEmptyResult);
    }
    let embeddings: BTreeMap<CxId, Vec<f32>> = rows
        .iter()
        .map(|row| (row.cx_id, row.vector.clone()))
        .collect();
    let corpus_size = rows.len();
    let corpus = InMemoryCorpus::new("vault-kernel", rows);

    Ok(VaultKernelInputs {
        kernel,
        embeddings,
        full,
        corpus,
        corpus_size,
        vault_corpus_size,
    })
}

fn build_vault_graph_ann_index(
    rows: &[RecallQuery],
    content_slot: PanelSlotId,
) -> Result<HnswIndex> {
    let dim = rows.first().map(|row| row.vector.len()).ok_or_else(|| {
        LodestarError::KernelInvalidParams {
            detail: "vault graph ANN index requires at least one embedded row".to_string(),
        }
    })?;
    let dim = u32::try_from(dim).map_err(|_| LodestarError::KernelIndexBuild {
        detail: format!("vault graph ANN dimension {dim} exceeds u32::MAX"),
    })?;
    let mut index = HnswIndex::new(content_slot.slot_id(), dim, VAULT_GRAPH_INDEX_SEED);
    for (seq, row) in rows.iter().enumerate() {
        SextantIndex::insert(
            &mut index,
            row.cx_id,
            SlotVector::Dense {
                dim,
                data: row.vector.clone(),
            },
            seq as u64,
        )
        .map_err(|error| LodestarError::KernelIndexBuild {
            detail: format!("build vault graph ANN index: {error}"),
        })?;
    }
    Ok(index)
}
