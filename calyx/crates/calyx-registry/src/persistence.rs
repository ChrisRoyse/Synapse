use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use calyx_aster::manifest::{ImmutableRef, ManifestStore, VaultManifest};
use calyx_core::{CalyxError, Input, Lens, LensId, Modality, Panel, Result, SlotShape, SlotVector};
use serde::{Deserialize, Serialize};

use crate::persistence_contracts::{contract_field_diffs, load_runtime_lens_from_spec};
use crate::{Registry, RegistryLensSnapshot};

const SNAPSHOT_VERSION: u16 = 1;

mod assets;
mod batch_limits;
mod lazy;
mod runtime;

pub(crate) use assets::load_manifest_panel_registry_snapshot;
pub use batch_limits::{apply_registry_snapshot_batch_limits, set_vault_registry_batch_limits};
pub(crate) use lazy::rebuild_registry;
pub use runtime::{
    LoadedRegistrySnapshotLens, measure_registry_snapshot_lens_batch,
    measure_registry_snapshot_lens_batch_with_stats,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VaultRegistrySnapshot {
    pub version: u16,
    pub panel_ref: ImmutableRef,
    pub lenses: Vec<RegistryLensSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultPanelWrite {
    pub manifest_seq: u64,
    pub durable_seq: u64,
    pub panel_ref: ImmutableRef,
    pub registry_ref: ImmutableRef,
}

#[derive(Clone)]
pub struct VaultPanelState {
    pub panel: Panel,
    pub registry: Registry,
    pub registry_snapshot: Option<VaultRegistrySnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrySnapshotMeasureStats {
    pub input_count: usize,
    pub runtime_batch_limit: Option<usize>,
    pub effective_chunk_size: usize,
    pub chunk_count: usize,
    pub runtime_load_ms: u128,
    pub measure_ms: u128,
    pub total_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryBatchLimitUpdate {
    pub lens_id: LensId,
    pub max_batch: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryBatchLimitChange {
    pub lens_id: LensId,
    pub name: String,
    pub before: Option<usize>,
    pub after: usize,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultRegistryBatchLimitWrite {
    pub manifest_seq: u64,
    pub durable_seq: u64,
    pub panel_ref: ImmutableRef,
    pub registry_ref: ImmutableRef,
    pub wrote_manifest: bool,
    pub changes: Vec<RegistryBatchLimitChange>,
}

pub fn persist_vault_panel_state(
    vault_dir: impl AsRef<Path>,
    panel: &Panel,
    registry: &Registry,
) -> Result<VaultPanelWrite> {
    let vault_dir = vault_dir.as_ref();
    let store = ManifestStore::open(vault_dir);
    let mut manifest = store.load_current()?;
    let panel_ref = assets::write_panel_asset(vault_dir, panel)?;
    let registry_ref = assets::write_registry_asset(vault_dir, &panel_ref, registry)?;
    manifest.manifest_seq = manifest
        .manifest_seq
        .checked_add(1)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("manifest sequence exhausted"))?;
    manifest.panel_ref = panel_ref.clone();
    manifest.registry_ref = Some(registry_ref.clone());
    manifest.validate()?;
    let durable_seq = manifest.durable_seq;
    let manifest_seq = manifest.manifest_seq;
    store.write_current(&manifest)?;
    Ok(VaultPanelWrite {
        manifest_seq,
        durable_seq,
        panel_ref,
        registry_ref,
    })
}

/// Stable subsystem-local code for a manifest that publishes no active panel.
///
/// This is distinct from [`CalyxError::aster_corrupt_shard`]: the manifest is
/// structurally sound but its `panel_ref` is the generated "no-active-panel"
/// placeholder, which is an expected empty state (nothing has been published
/// yet), not shard corruption. The remediation names the publication step and
/// deliberately does not advise restore-from-backup.
pub const CALYX_NO_ACTIVE_PANEL: &str = "CALYX_NO_ACTIVE_PANEL";

/// `kind` field stamped into every generated manifest placeholder asset.
const GENERATED_ASSET_KIND: &str = "calyx_manifest_generated_asset_v1";
/// `status` field of the generated no-active-panel placeholder.
const NO_ACTIVE_PANEL_STATUS: &str = "no-active-panel";

/// Returns true when `bytes` is the generated no-active-panel placeholder that
/// Aster writes into a fresh manifest before any real `Panel` is published.
fn is_no_active_panel_placeholder(bytes: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct GeneratedAssetMarker {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        status: String,
    }
    serde_json::from_slice::<GeneratedAssetMarker>(bytes).is_ok_and(|marker| {
        marker.kind == GENERATED_ASSET_KIND && marker.status == NO_ACTIVE_PANEL_STATUS
    })
}

/// Builds the typed no-active-panel error for a placeholder `panel_ref`.
fn no_active_panel_error(logical_path: &str) -> CalyxError {
    CalyxError {
        code: CALYX_NO_ACTIVE_PANEL,
        message: format!(
            "durable manifest publishes no active panel: panel_ref {logical_path} is the generated no-active-panel placeholder, so there is nothing to load or rebuild"
        ),
        remediation: "publish an active durable Panel snapshot (persist_vault_panel_state) for the live constellation panel before search rebuild; this is an expected empty state, not shard corruption \u{2014} do not restore from restic/snapshot",
    }
}

pub fn load_vault_panel_state(vault_dir: impl AsRef<Path>) -> Result<VaultPanelState> {
    let vault_dir = vault_dir.as_ref();
    let manifest = ManifestStore::open(vault_dir).load_current()?;
    let panel_bytes = assets::read_ref(vault_dir, &manifest.panel_ref)?;
    if is_no_active_panel_placeholder(&panel_bytes) {
        return Err(no_active_panel_error(&manifest.panel_ref.logical_path));
    }
    let panel: Panel = serde_json::from_slice(&panel_bytes)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("decode panel: {error}")))?;
    let snapshot = manifest
        .registry_ref
        .as_ref()
        .map(|reference| assets::read_registry_snapshot(vault_dir, reference, &manifest.panel_ref))
        .transpose()?;
    let registry = snapshot
        .as_ref()
        .map_or_else(|| Ok(Registry::new()), lazy::rebuild_registry)?;
    Ok(VaultPanelState {
        panel,
        registry,
        registry_snapshot: snapshot,
    })
}
