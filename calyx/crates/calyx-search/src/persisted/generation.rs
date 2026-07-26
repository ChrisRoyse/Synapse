use calyx_core::{CalyxError, PanelSlotId, SlotId, SlotShape};
use serde::Serialize;

use super::{PersistedSearchIndexes, SearchIndexEntry};
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
        Ok(Self {
            panel_slot: PanelSlotId::new(panel_version, SlotId::new(entry.slot)),
            kind: entry.kind.clone(),
            shape,
            len: entry.len,
            built_at_seq: entry.built_at_seq,
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
