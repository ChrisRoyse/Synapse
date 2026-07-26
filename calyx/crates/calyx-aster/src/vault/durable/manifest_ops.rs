use super::{DurableVault, storage_error};
use crate::manifest::{ImmutableRef, ManifestStore, VaultManifest};
use crate::timetravel::RetentionHorizon;
use calyx_core::{CalyxError, Panel, Result};
use std::fs;
use std::io;
use std::path::Path;

impl DurableVault {
    pub(super) fn retention_horizon(&self) -> RetentionHorizon {
        self.retention_horizon
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub(in crate::vault) fn write_retention_horizon_manifest(
        &self,
        horizon: &RetentionHorizon,
    ) -> Result<()> {
        horizon.validate()?;
        let current = self.current_manifest()?;
        let manifest_seq = current
            .as_ref()
            .map_or(1, |manifest| manifest.manifest_seq.saturating_add(1));
        let durable_seq = current.as_ref().map_or(0, |manifest| manifest.durable_seq);
        self.write_manifest_with_seq_and_horizon(manifest_seq, durable_seq, horizon)?;
        *self
            .retention_horizon
            .lock()
            .map_err(|_| CalyxError::backpressure("retention horizon lock poisoned"))? =
            horizon.clone();
        Ok(())
    }

    /// Durable coverage watermark from the on-disk CURRENT manifest, or 0
    /// before the first manifest write.
    pub(in crate::vault) fn manifest_durable_seq(&self) -> Result<u64> {
        Ok(self
            .current_manifest()?
            .map_or(0, |manifest| manifest.durable_seq))
    }

    pub(super) fn write_manifest(&self, seq: u64) -> Result<()> {
        let manifest_seq = self.current_manifest()?.map_or(seq.max(1), |manifest| {
            manifest.manifest_seq.saturating_add(1)
        });
        self.write_manifest_with_seq(manifest_seq, seq)
    }

    pub(super) fn write_manifest_with_seq(
        &self,
        manifest_seq: u64,
        durable_seq: u64,
    ) -> Result<()> {
        let horizon = self.retention_horizon();
        self.write_manifest_with_seq_and_horizon(manifest_seq, durable_seq, &horizon)
    }

    fn write_manifest_with_seq_and_horizon(
        &self,
        manifest_seq: u64,
        durable_seq: u64,
        horizon: &RetentionHorizon,
    ) -> Result<()> {
        let current = self.current_manifest()?;
        // Durable coverage is monotone: the WAL replay floor (and segment
        // recycling) derive from `durable_seq`, so letting a stale handle
        // regress it would resurrect a floor below already-recycled WAL
        // records and lose their rows (issue #1132).
        let durable_seq = current.as_ref().map_or(durable_seq, |manifest| {
            manifest.durable_seq.max(durable_seq)
        });
        let (panel_ref, codebook_refs) = match (&self.panel, current.as_ref()) {
            (Some(panel), _) => ensure_manifest_assets(&self.root, Some(panel))?,
            (None, Some(manifest)) => (manifest.panel_ref.clone(), manifest.codebook_refs.clone()),
            (None, None) => ensure_manifest_assets(&self.root, None)?,
        };
        let mut manifest = VaultManifest::new_with_policies(
            manifest_seq,
            durable_seq,
            panel_ref,
            codebook_refs,
            self.temporal_policy,
            self.dedup_policy.clone(),
        )?;
        // The local atomic only knows about content THIS handle checkpointed.
        // A foreign writer (second handle/process) may have recorded a higher
        // watermark in the current manifest; a content-neutral write from this
        // handle must never regress it, or a genuinely stale index would pass
        // freshness (#1100 review finding).
        let mut derived_content_seq = self.derived_content_seq_for_manifest(durable_seq);
        if let Some(current) = current
            .as_ref()
            .filter(|manifest| manifest.uses_persistent_search_content_model())
        {
            derived_content_seq =
                derived_content_seq.max(current.effective_derived_content_seq().min(durable_seq));
        }
        manifest.derived_content_seq = Some(derived_content_seq);
        let mut panel_content_seqs = self.panel_content_seqs_for_manifest(durable_seq)?;
        if let Some(current) = current
            .as_ref()
            .filter(|manifest| manifest.uses_persistent_search_content_model())
        {
            for (panel_version, seq) in &current.panel_content_seqs {
                let watermark = panel_content_seqs.entry(*panel_version).or_default();
                *watermark = (*watermark).max((*seq).min(durable_seq));
            }
        }
        manifest.panel_content_seqs = panel_content_seqs;
        manifest.retention_horizon = horizon.clone();
        manifest.registry_ref = current.and_then(|manifest| manifest.registry_ref);
        manifest.validate()?;
        ManifestStore::open(&self.root).write_current(&manifest)?;
        Ok(())
    }

    fn current_manifest(&self) -> Result<Option<VaultManifest>> {
        if self.root.join("CURRENT").exists() {
            ManifestStore::open(&self.root).load_current().map(Some)
        } else {
            Ok(None)
        }
    }
}

fn ensure_manifest_assets(
    root: &Path,
    panel: Option<&Panel>,
) -> Result<(ImmutableRef, Vec<ImmutableRef>)> {
    let panel_ref = if let Some(panel) = panel {
        let panel_bytes = serde_json::to_vec_pretty(panel).map_err(|error| {
            CalyxError::aster_corrupt_shard(format!("encode durable panel asset: {error}"))
        })?;
        let hash = blake3::hash(&panel_bytes).to_hex().to_string();
        let logical = format!("panel/panel-v{:08}-{}.json", panel.version, &hash[..16]);
        write_asset(&root.join(&logical), &panel_bytes)?;
        ImmutableRef::from_bytes(logical, &panel_bytes)?
    } else {
        let panel_bytes = generated_asset_bytes("panel", "no-active-panel");
        let hash = blake3::hash(&panel_bytes).to_hex().to_string();
        let logical = format!("panel/generated-no-active-panel-{}.json", &hash[..16]);
        write_asset(&root.join(&logical), &panel_bytes)?;
        ImmutableRef::from_bytes(logical, &panel_bytes)?
    };
    let codebook_bytes = generated_asset_bytes("codebook", "no-active-codebook");
    let hash = blake3::hash(&codebook_bytes).to_hex().to_string();
    let codebook_logical = format!(
        "codebooks/generated-no-active-codebook-{}.json",
        &hash[..16]
    );
    write_asset(&root.join(&codebook_logical), &codebook_bytes)?;
    Ok((
        panel_ref,
        vec![ImmutableRef::from_bytes(codebook_logical, &codebook_bytes)?],
    ))
}

fn generated_asset_bytes(kind: &str, status: &str) -> Vec<u8> {
    serde_json::json!({
        "kind": "calyx_manifest_generated_asset_v1",
        "asset_kind": kind,
        "status": status
    })
    .to_string()
    .into_bytes()
}

fn write_asset(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(()),
        Ok(_) => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "manifest immutable asset {} hash mismatch",
                path.display()
            )));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(storage_error("read manifest asset", error));
        }
        Err(_) => {}
    }
    if let Some(parent) = path.parent() {
        crate::fsync::create_dir_all(parent, "manifest asset")?;
    }
    crate::fsync::write_atomic_create_new(path, bytes, "manifest asset")
}
