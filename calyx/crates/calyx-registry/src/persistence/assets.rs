use super::*;

pub(super) fn write_panel_asset(vault_dir: &Path, panel: &Panel) -> Result<ImmutableRef> {
    let bytes = serde_json::to_vec_pretty(panel)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("encode panel: {error}")))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let logical = format!("panel/panel-v{:08}-{}.json", panel.version, &hash[..16]);
    write_asset(&vault_dir.join(&logical), &bytes)?;
    ImmutableRef::from_bytes(logical, &bytes)
}

pub(super) fn write_registry_asset(
    vault_dir: &Path,
    panel_ref: &ImmutableRef,
    registry: &Registry,
) -> Result<ImmutableRef> {
    let snapshot = VaultRegistrySnapshot {
        version: SNAPSHOT_VERSION,
        panel_ref: panel_ref.clone(),
        lenses: registry.lens_snapshots(),
    };
    let bytes = serde_json::to_vec_pretty(&snapshot)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("encode registry: {error}")))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let logical = format!("registry/registry-{}.json", &hash[..16]);
    write_asset(&vault_dir.join(&logical), &bytes)?;
    ImmutableRef::from_bytes(logical, &bytes)
}

pub(super) fn read_registry_snapshot(
    vault_dir: &Path,
    reference: &ImmutableRef,
    panel_ref: &ImmutableRef,
) -> Result<VaultRegistrySnapshot> {
    let bytes = read_ref(vault_dir, reference)?;
    let snapshot: VaultRegistrySnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("decode registry: {error}")))?;
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "unsupported registry snapshot version {}",
            snapshot.version
        )));
    }
    if &snapshot.panel_ref != panel_ref {
        return Err(CalyxError::aster_corrupt_shard(
            "registry snapshot panel_ref does not match manifest panel_ref",
        ));
    }
    Ok(snapshot)
}

pub(crate) fn load_manifest_panel_registry_snapshot(
    vault_dir: &Path,
) -> Result<(VaultManifest, Panel, VaultRegistrySnapshot)> {
    let manifest = ManifestStore::open(vault_dir).load_current()?;
    let panel_bytes = read_ref(vault_dir, &manifest.panel_ref)?;
    if super::is_no_active_panel_placeholder(&panel_bytes) {
        return Err(super::no_active_panel_error(
            &manifest.panel_ref.logical_path,
        ));
    }
    let panel: Panel = serde_json::from_slice(&panel_bytes)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("decode panel: {error}")))?;
    let registry_ref = manifest.registry_ref.as_ref().ok_or_else(|| {
        CalyxError::aster_corrupt_shard("vault has no persisted registry snapshot")
    })?;
    let snapshot = read_registry_snapshot(vault_dir, registry_ref, &manifest.panel_ref)?;
    Ok((manifest, panel, snapshot))
}

pub(super) fn read_ref(vault_dir: &Path, reference: &ImmutableRef) -> Result<Vec<u8>> {
    fs::read(vault_dir.join(&reference.logical_path)).map_err(|error| {
        CalyxError::aster_corrupt_shard(format!(
            "manifest ref {} unreadable: {error}",
            reference.logical_path
        ))
    })
}

pub(crate) fn write_asset(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(()),
        Ok(_) => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "registry immutable asset {} hash mismatch",
                path.display()
            )));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(storage_error("read registry asset", error));
        }
        Err(_) => {}
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| storage_error("create registry asset dir", error))?;
    }
    let tmp = tmp_path(path);
    {
        let mut file =
            File::create(&tmp).map_err(|error| storage_error("create registry asset", error))?;
        file.write_all(bytes)
            .map_err(|error| storage_error("write registry asset", error))?;
        file.sync_all()
            .map_err(|error| storage_error("fsync registry asset", error))?;
    }
    fs::rename(&tmp, path).map_err(|error| storage_error("install registry asset", error))
}

fn tmp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("registry-asset");
    path.with_file_name(format!(
        ".{file_name}.{:?}.tmp",
        std::thread::current().id()
    ))
}

fn storage_error(context: &str, error: io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!("{context}: {error}"))
}
