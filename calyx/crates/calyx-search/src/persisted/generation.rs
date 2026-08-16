use calyx_core::{CalyxError, PanelSlotId, SlotId, SlotShape};
use serde::Serialize;

use super::{PersistedDenseIndexConfig, PersistedSearchIndexes, SearchIndexEntry};
use crate::error::CliResult;

/// Public, path-free identity for one immutable persisted search generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PersistedSearchGeneration {
    pub panel_version: u32,
    pub base_seq: u64,
    pub manifest_sha256: String,
    pub diskann_build_backend: Option<String>,
    pub diskann_build_backend_source: Option<String>,
    pub sextant_cuvs_compiled: Option<bool>,
    pub sextant_cuda_pq_compiled: Option<bool>,
    pub dense_index_config: PersistedDenseIndexConfig,
    pub slots: Vec<PersistedSearchSlot>,
}

/// Search-relevant manifest data for one persisted slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PersistedSearchSlot {
    pub panel_slot: PanelSlotId,
    pub kind: String,
    pub shape: SlotShape,
    pub len: usize,
    pub built_at_seq: u64,
    /// The exact within-lane scoring law this index ranks by (#1900).
    ///
    /// Reported rather than inferred. `sparse_dot` and `sparse_bm25` are both
    /// "the sparse lane" and rank by entirely different laws — one of them with
    /// no IDF and no length saturation at all — so a caller reading a rank has
    /// to be told which one produced it.
    pub scoring_law: String,
    pub quantization: Option<PersistedDenseQuantization>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PersistedDenseQuantization {
    pub bits: u8,
    pub subvectors: usize,
    pub centroids: usize,
    pub pq_sha256: String,
    pub raw_sha256: String,
}

/// The within-lane scoring law for one persisted index kind.
///
/// Single source of truth: the strings are derived from the scorers themselves
/// (`Bm25::law`) or state the operation the lane physically performs, so they
/// cannot drift from the code that ranks.
#[must_use]
pub fn slot_scoring_law(kind: &str) -> String {
    match kind {
        "flat_dense" => {
            "cosine: score(d) = <q,d> / (||q||*||d||) over the exhaustive dense lane".to_owned()
        }
        "diskann" => {
            "cosine: score(d) = <q,d> / (||q||*||d||) over a Vamana graph, approximate by construction".to_owned()
        }
        "sparse_bm25" | "sparse_inverted" => calyx_sextant::index::bm25::Bm25::default().law(),
        "sparse_dot" => {
            "dot: score(d) = SUM_t q_t * d_t over stored sparse weights; NO idf and NO document-length saturation, so a term shared with a short document can outscore a full exact match (#1900)".to_owned()
        }
        "multi_maxsim" | "multi_maxsim_segments" => {
            "maxsim: score(d) = SUM_i max_j <q_i, d_j> late interaction over token vectors".to_owned()
        }
        other => format!("unknown index kind {other}: no declared scoring law"),
    }
}

impl PersistedSearchIndexes {
    /// Returns a validated, path-free descriptor suitable for runtime wiring
    /// and operational evidence. Malformed slot shapes fail instead of being
    /// guessed from stored rows.
    pub fn generation(&self) -> CliResult<PersistedSearchGeneration> {
        let slots = self
            .manifest
            .slots
            .iter()
            .map(|entry| PersistedSearchSlot::from_entry(self.manifest.panel_version, entry))
            .collect::<CliResult<Vec<_>>>()?;
        Ok(PersistedSearchGeneration {
            panel_version: self.manifest.panel_version,
            base_seq: self.manifest.base_seq,
            manifest_sha256: self.manifest_sha256.clone(),
            diskann_build_backend: self.manifest.diskann_build_backend.clone(),
            diskann_build_backend_source: self.manifest.diskann_build_backend_source.clone(),
            sextant_cuvs_compiled: self.manifest.sextant_cuvs_compiled,
            sextant_cuda_pq_compiled: self.manifest.sextant_cuda_pq_compiled,
            dense_index_config: self.manifest.dense_index_config.clone(),
            slots,
        })
    }
}

impl PersistedSearchSlot {
    fn from_entry(panel_version: u32, entry: &SearchIndexEntry) -> CliResult<Self> {
        let shape = match entry.kind.as_str() {
            "diskann" | "flat_dense" => SlotShape::Dense(required_dim(entry)?),
            "sparse_inverted" | "sparse_bm25" | "sparse_dot" => {
                SlotShape::Sparse(required_dim(entry)?)
            }
            "multi_maxsim" | "multi_maxsim_segments" => SlotShape::Multi {
                token_dim: entry
                    .token_dim
                    .ok_or_else(|| malformed(entry, "token_dim"))?,
            },
            kind => {
                return Err(CalyxError::stale_derived(format!(
                    "persistent search slot {} has unknown kind {kind}",
                    entry.slot
                ))
                .into());
            }
        };
        let mut scoring_law = slot_scoring_law(&entry.kind);
        if let Some(quantization) = &entry.dense_quantization {
            scoring_law.push_str(&format!(
                "; candidate distances use {}-bit PQ ({} subvectors, {} centroids) and final ordering is exact cosine reranking from the hash-bound packed raw sidecar",
                quantization.bits, quantization.subvectors, quantization.centroids
            ));
        }
        Ok(Self {
            panel_slot: PanelSlotId::new(panel_version, SlotId::new(entry.slot)),
            scoring_law,
            kind: entry.kind.clone(),
            shape,
            len: entry.len,
            built_at_seq: entry.built_at_seq,
            quantization: entry.dense_quantization.as_ref().map(|quantization| {
                PersistedDenseQuantization {
                    bits: quantization.bits,
                    subvectors: quantization.subvectors,
                    centroids: quantization.centroids,
                    pq_sha256: quantization.pq_sha256.clone(),
                    raw_sha256: quantization.raw_sha256.clone(),
                }
            }),
        })
    }
}

fn required_dim(entry: &SearchIndexEntry) -> CliResult<u32> {
    entry.dim.ok_or_else(|| malformed(entry, "dim").into())
}

fn malformed(entry: &SearchIndexEntry, field: &str) -> CalyxError {
    CalyxError::stale_derived(format!(
        "persistent search slot {} kind {} is missing {field}",
        entry.slot, entry.kind
    ))
}
