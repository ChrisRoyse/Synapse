use calyx_aster::cf::ColumnFamily;
use calyx_aster::mvcc::Snapshot;
use calyx_aster::vault::encode::{decode_constellation_base, decode_slot_vector};
use calyx_core::{CalyxError, Clock, CxId, PanelSlotId, SlotId, SlotShape, SlotState};
use calyx_registry::{LensRuntime, VaultPanelState};
use rayon::prelude::*;

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildProgress<'a> {
    pub phase: &'static str,
    pub panel_slot: Option<PanelSlotId>,
    pub rows: Option<usize>,
    pub base_seq: Option<u64>,
    pub manifest_path: Option<&'a Path>,
    /// Free-form context for exceptional events, e.g. why prior-segment
    /// reuse was declined during a rebuild (#1109).
    pub detail: Option<String>,
}

impl<'a> RebuildProgress<'a> {
    pub(super) fn phase(phase: &'static str) -> Self {
        Self {
            phase,
            panel_slot: None,
            rows: None,
            base_seq: None,
            manifest_path: None,
            detail: None,
        }
    }

    pub(super) fn slot(
        phase: &'static str,
        panel_version: u32,
        slot: SlotId,
        rows: Option<usize>,
        base_seq: Option<u64>,
    ) -> Self {
        Self {
            phase,
            panel_slot: Some(PanelSlotId::new(panel_version, slot)),
            rows,
            base_seq,
            manifest_path: None,
            detail: None,
        }
    }

    pub(super) fn manifest(phase: &'static str, manifest_path: &'a Path, base_seq: u64) -> Self {
        Self {
            phase,
            panel_slot: None,
            rows: None,
            base_seq: Some(base_seq),
            manifest_path: Some(manifest_path),
            detail: None,
        }
    }
}

pub fn rebuild_for_vault<C: Clock>(vault_dir: &Path, vault: &AsterVault<C>) -> CliResult {
    rebuild_for_vault_with_progress(vault_dir, vault, |_| {})
}

pub fn rebuild_for_vault_with_panel_state<C: Clock>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    state: &calyx_registry::VaultPanelState,
) -> CliResult {
    rebuild_for_vault_with_panel_state_progress(vault_dir, vault, state, |_| {})
}

pub fn rebuild_for_vault_with_progress<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    mut progress: F,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) + Send,
{
    rebuild_for_vault_with_fallible_progress(vault_dir, vault, |event| {
        progress(event);
        Ok(())
    })
}

pub fn rebuild_for_vault_with_panel_state_progress<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    state: &calyx_registry::VaultPanelState,
    mut progress: F,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) + Send,
{
    rebuild_for_vault_with_panel_state_fallible_progress(vault_dir, vault, state, |event| {
        progress(event);
        Ok(())
    })
}

pub fn rebuild_for_vault_with_fallible_progress<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    progress: F,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    let state = calyx_registry::load_vault_panel_state(vault_dir)?;
    rebuild_for_vault_with_panel_state_fallible_progress(vault_dir, vault, &state, progress)
}

pub fn rebuild_for_vault_with_panel_state_fallible_progress<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    state: &VaultPanelState,
    progress: F,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    let active_slots = active_panel_slots(state);
    let sparse_scoring = active_sparse_scoring(state)?;
    super::rebuild_stream::rebuild_for_vault_with_active_slots_progress(
        vault_dir,
        vault,
        state.panel.version,
        &active_slots,
        &sparse_scoring,
        progress,
    )
}

fn active_panel_slots(state: &VaultPanelState) -> BTreeSet<SlotId> {
    state
        .panel
        .slots
        .iter()
        .filter(|slot| slot.state == SlotState::Active)
        .map(|slot| slot.slot_id)
        .collect()
}

fn active_sparse_scoring(
    state: &VaultPanelState,
) -> CliResult<BTreeMap<SlotId, sparse::SparseScoring>> {
    let mut scoring = BTreeMap::new();
    for slot in state
        .panel
        .slots
        .iter()
        .filter(|slot| slot.state == SlotState::Active)
    {
        if !matches!(slot.shape, SlotShape::Sparse(_)) {
            continue;
        }
        let spec = state.registry.lens_spec(slot.lens_id).ok_or_else(|| {
            stale(format!(
                "active sparse panel slot {} references lens {}, but the persisted registry has no LensSpec; repair the panel registry before rebuilding search indexes",
                slot.slot_id, slot.lens_id
            ))
        })?;
        if spec.output != slot.shape {
            return Err(stale(format!(
                "active sparse panel slot {} shape {:?} != lens {} contract {:?}; repair the panel registry before rebuilding search indexes",
                slot.slot_id, slot.shape, slot.lens_id, spec.output
            )));
        }
        scoring.insert(slot.slot_id, sparse_scoring_for_runtime(&spec.runtime));
    }
    Ok(scoring)
}

fn sparse_scoring_for_runtime(runtime: &LensRuntime) -> sparse::SparseScoring {
    match runtime {
        LensRuntime::Algorithmic { kind } if is_lexical_term_frequency_kind(kind) => {
            sparse::SparseScoring::Bm25
        }
        _ => sparse::SparseScoring::DotProduct,
    }
}

fn is_lexical_term_frequency_kind(kind: &str) -> bool {
    let normalized = kind.replace('-', "_");
    matches!(
        normalized.split(':').next(),
        Some("sparse" | "sparse_keywords")
    )
}

pub(super) fn previous_manifest(
    vault_dir: &Path,
    panel_version: u32,
) -> CliResult<Option<SearchIndexManifest>> {
    let path = manifest_path(vault_dir, panel_version);
    if !path.exists() {
        return Ok(None);
    }
    let manifest: SearchIndexManifest =
        serde_json::from_slice(&fs::read(&path)?).map_err(|err| {
            stale(format!(
                "persistent search index manifest {} is unreadable before rebuild: {err}",
                path.display()
            ))
        })?;
    if manifest.format != MANIFEST_FORMAT {
        return Err(stale(format!(
            "persistent search index manifest {} has format {}; expected {MANIFEST_FORMAT}",
            path.display(),
            manifest.format
        )));
    }
    if manifest.panel_version != panel_version {
        return Err(stale(format!(
            "persistent search index manifest {} declares panel {} but panel {panel_version} was requested",
            path.display(),
            manifest.panel_version
        )));
    }
    Ok(Some(manifest))
}

pub fn load_docs<C: Clock>(vault: &AsterVault<C>) -> CliResult<BTreeMap<CxId, Constellation>> {
    let snapshot = vault.pin_reader(calyx_aster::mvcc::Freshness::FreshDerived, 300_000);
    let _guard = PinnedReadGuard::new(vault, snapshot);
    load_docs_at(vault, _guard.snapshot())
}

pub fn load_docs_at<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: Snapshot,
) -> CliResult<BTreeMap<CxId, Constellation>> {
    load_docs_for_panel_at(vault, snapshot, None)
}

pub(crate) fn load_docs_for_panel_at<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    requested_panel_version: Option<u32>,
) -> CliResult<BTreeMap<CxId, Constellation>> {
    let base_rows = vault.scan_cf_snapshot(snapshot, ColumnFamily::Base)?;
    let decoded_base = base_rows
        .into_par_iter()
        .map(|(key, bytes)| {
            let cx_id = cx_id_from_cf_key(&key, "base CF")?;
            let cx = decode_constellation_base(&bytes)?;
            if cx.cx_id != cx_id {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "base CF key {cx_id} contains constellation {}",
                    cx.cx_id
                )));
            }
            Ok((cx_id, cx))
        })
        .collect::<calyx_core::Result<Vec<_>>>()?;
    let panel_versions = decoded_base
        .iter()
        .map(|(_, cx)| cx.panel_version)
        .collect::<BTreeSet<_>>();
    let panel_version = match requested_panel_version {
        Some(panel_version) => panel_version,
        None if panel_versions.len() == 1 => *panel_versions
            .first()
            .expect("one panel version was observed"),
        None if panel_versions.is_empty() => return Ok(BTreeMap::new()),
        None => {
            return Err(CalyxError {
                code: CALYX_SEARCH_PANEL_SCOPE_REQUIRED,
                message: format!(
                    "vault snapshot {} contains {} panel versions {:?}; a bare SlotId is ambiguous",
                    snapshot.seq(),
                    panel_versions.len(),
                    panel_versions
                ),
                remediation: "supply the exact panel version and rebuild/search only that panel generation",
            }
            .into());
        }
    };
    let mut docs = decoded_base
        .into_iter()
        .filter(|(_, cx)| cx.panel_version == panel_version)
        .collect::<BTreeMap<_, _>>();
    let slots = indexed_slots(&docs);
    for slot in slots {
        load_slot_rows(vault, snapshot, slot, &mut docs)?;
    }
    Ok(docs)
}

struct PinnedReadGuard<'a, C: Clock> {
    vault: &'a AsterVault<C>,
    snapshot: Snapshot,
}

impl<'a, C: Clock> PinnedReadGuard<'a, C> {
    fn new(vault: &'a AsterVault<C>, snapshot: Snapshot) -> Self {
        Self { vault, snapshot }
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot
    }
}

impl<C: Clock> Drop for PinnedReadGuard<'_, C> {
    fn drop(&mut self) {
        let _ = self.vault.release_reader(self.snapshot.lease().id());
    }
}

fn indexed_slots(docs: &BTreeMap<CxId, Constellation>) -> Vec<SlotId> {
    let mut slots = docs
        .values()
        .flat_map(|cx| cx.slots.keys().copied())
        .collect::<Vec<_>>();
    slots.sort();
    slots.dedup();
    slots
}

fn load_slot_rows<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    slot: SlotId,
    docs: &mut BTreeMap<CxId, Constellation>,
) -> CliResult {
    let expected = docs
        .iter()
        .filter_map(|(cx_id, cx)| cx.slots.contains_key(&slot).then_some(*cx_id))
        .collect::<std::collections::BTreeSet<_>>();
    let rows = vault.scan_cf_snapshot(snapshot, ColumnFamily::slot(slot))?;
    let decoded = rows
        .into_par_iter()
        .map(|(key, bytes)| {
            let cx_id = cx_id_from_cf_key(&key, "slot CF")?;
            let vector = decode_slot_vector(&bytes)?;
            Ok((cx_id, vector))
        })
        .collect::<calyx_core::Result<Vec<_>>>()?;
    let mut found = std::collections::BTreeSet::new();
    for (cx_id, vector) in decoded {
        if !expected.contains(&cx_id) {
            continue;
        }
        let Some(cx) = docs.get_mut(&cx_id) else {
            continue;
        };
        cx.slots.insert(slot, vector);
        found.insert(cx_id);
    }
    if found.len() != expected.len() {
        let missing = expected
            .difference(&found)
            .next()
            .map(ToString::to_string)
            .unwrap_or_else(|| "<unknown>".to_string());
        return Err(CalyxError::aster_corrupt_shard(format!(
            "slot CF row missing for slot {slot} cx_id {missing}"
        ))
        .into());
    }
    Ok(())
}

fn cx_id_from_cf_key(key: &[u8], cf_name: &str) -> calyx_core::Result<CxId> {
    let bytes: [u8; 16] = key.try_into().map_err(|_| {
        CalyxError::vault_access_denied(format!("{cf_name} key has {} bytes", key.len()))
    })?;
    Ok(CxId::from_bytes(bytes))
}

pub(super) fn prune_stale_index_artifacts(
    vault_dir: &Path,
    root: &Path,
    manifest: &SearchIndexManifest,
) -> CliResult {
    let keep = referenced_index_artifacts(vault_dir, root, manifest)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_prunable_index_artifact(&name) || keep.iter().any(|item| item == &path) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn referenced_index_artifacts(
    vault_dir: &Path,
    root: &Path,
    manifest: &SearchIndexManifest,
) -> CliResult<Vec<PathBuf>> {
    let mut keep = vec![manifest_path(vault_dir, manifest.panel_version)];
    if let Some(filter) = &manifest.filter {
        keep.push(vault_dir.join(&filter.index_rel));
    }
    for entry in &manifest.slots {
        if let Some(index_rel) = &entry.index_rel {
            keep.push(vault_dir.join(index_rel));
            if entry.kind == "multi_maxsim_segments" {
                keep.extend(multi::referenced_segment_artifacts(
                    vault_dir,
                    entry,
                    SlotId::new(entry.slot),
                )?);
            }
        }
        if let Some(graph_rel) = &entry.graph_rel {
            let graph = vault_dir.join(graph_rel);
            let ann_dir = graph.parent().ok_or_else(|| {
                stale(format!(
                    "persistent slot {} graph path has no parent directory",
                    entry.slot
                ))
            })?;
            if ann_dir.parent().is_some_and(|parent| parent == root) {
                keep.push(ann_dir.to_path_buf());
            } else {
                keep.push(graph);
            }
        }
        if let Some(id_map_rel) = &entry.id_map_rel {
            keep.push(vault_dir.join(id_map_rel));
        }
    }
    keep.sort();
    keep.dedup();
    Ok(keep)
}

fn is_prunable_index_artifact(name: &str) -> bool {
    name.starts_with("slot_") || name.starts_with("filter_") || name.starts_with("filters_")
}
