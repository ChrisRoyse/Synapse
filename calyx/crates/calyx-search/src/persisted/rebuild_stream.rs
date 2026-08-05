use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use calyx_core::{Clock, PanelSlotId, SlotId};

use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::vault::AsterVault;
use rayon::prelude::*;

#[path = "rebuild_scan.rs"]
mod rebuild_scan;
use rebuild_scan::{
    LoadedBaseDocs, ScannedSlotRows, collect_or_build_slot_from_cf, load_base_docs_at,
};

use super::rebuild::{RebuildProgress, previous_manifest, prune_stale_index_artifacts};
use super::rebuild_plan::{
    DiskAnnBuildPolicy, SlotBuildPlan, bounded_parallel_slot_count,
    configured_diskann_build_policy, configured_rebuild_reader_lease_ms,
    configured_rebuild_scan_page_rows, manifest_backend, slot_build_plans,
    validate_parallel_rebuild_config,
};
use super::*;

pub(super) type SharedRebuildProgress<'a, F> = Arc<Mutex<&'a mut F>>;

pub(super) fn emit_shared_progress<F>(
    progress: &SharedRebuildProgress<'_, F>,
    event: RebuildProgress<'_>,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    let mut progress = progress
        .lock()
        .map_err(|_| stale("search rebuild progress sink lock poisoned"))?;
    (**progress)(event)
}

pub(super) fn rebuild_for_vault_with_active_slots_progress<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    panel_version: u32,
    active_slots: &BTreeSet<SlotId>,
    sparse_scoring: &BTreeMap<SlotId, sparse::SparseScoring>,
    dense_index_config: PersistedDenseIndexConfig,
    progress: F,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    rebuild_for_vault_with_slot_filter(
        vault_dir,
        vault,
        Some(panel_version),
        Some(active_slots),
        sparse_scoring,
        dense_index_config,
        progress,
    )
}

fn rebuild_for_vault_with_slot_filter<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    requested_panel_version: Option<u32>,
    active_slots: Option<&BTreeSet<SlotId>>,
    sparse_scoring: &BTreeMap<SlotId, sparse::SparseScoring>,
    dense_index_config: PersistedDenseIndexConfig,
    mut progress: F,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    validate_parallel_rebuild_config()?;
    let snapshot = vault.pin_reader(
        Freshness::FreshDerived,
        configured_rebuild_reader_lease_ms()?,
    );
    let guard = PinnedReadGuard::new(vault, snapshot);
    let base_seq = guard.snapshot().seq();
    progress(RebuildProgress::phase("load_docs_start"))?;
    let page_rows = configured_rebuild_scan_page_rows()?;
    let base_docs = load_base_docs_at(
        vault,
        guard.snapshot(),
        page_rows,
        requested_panel_version,
        &mut progress,
    )?;
    let panel_version = base_docs.panel_version();
    let retained_slot_payloads = base_docs.retained_slot_payloads();
    if retained_slot_payloads != 0 {
        return Err(stale(format!(
            "search rebuild retained {retained_slot_payloads} duplicate Base slot payloads after planning"
        )));
    }
    progress(RebuildProgress {
        rows: Some(base_docs.len()),
        base_seq: Some(base_seq),
        detail: Some(format!(
            "slot_memberships={} retained_base_slot_payloads={retained_slot_payloads}",
            base_docs.slot_memberships()
        )),
        ..RebuildProgress::phase("load_docs_ok")
    })?;
    let build_policy = configured_diskann_build_policy()?;
    let summary = rebuild_from_base_with_progress(
        vault_dir,
        vault,
        guard.snapshot(),
        &base_docs,
        RebuildOptions {
            page_rows,
            panel_version,
            active_slots,
            sparse_scoring,
            build_policy,
            dense_index_config,
        },
        &mut progress,
    )?;
    progress(RebuildProgress {
        rows: Some(summary.total_rows),
        base_seq: Some(base_seq),
        manifest_path: Some(&summary.manifest_path),
        ..RebuildProgress::phase("done")
    })?;
    let _ = (summary.slots, summary.total_rows, &summary.manifest_path);
    Ok(())
}

#[derive(Clone, Copy)]
struct RebuildOptions<'a> {
    page_rows: usize,
    panel_version: u32,
    active_slots: Option<&'a BTreeSet<SlotId>>,
    sparse_scoring: &'a BTreeMap<SlotId, sparse::SparseScoring>,
    build_policy: DiskAnnBuildPolicy,
    dense_index_config: PersistedDenseIndexConfig,
}

fn rebuild_from_base_with_progress<C: Clock, F>(
    vault_dir: &Path,
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    base_docs: &LoadedBaseDocs,
    options: RebuildOptions<'_>,
    progress: &mut F,
) -> CliResult<RebuildSummary>
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    let RebuildOptions {
        page_rows,
        panel_version,
        active_slots,
        sparse_scoring,
        build_policy,
        dense_index_config,
    } = options;
    let root = panel_index_root(vault_dir, panel_version);
    fs::create_dir_all(&root)?;
    let base_seq = snapshot.seq();
    progress(RebuildProgress::phase("previous_manifest_start"))?;
    let previous_manifest = previous_manifest(vault_dir, panel_version)?;
    progress(RebuildProgress::phase("previous_manifest_ok"))?;

    let plans = slot_build_plans(
        panel_version,
        &base_docs.ids_by_slot,
        previous_manifest.as_ref(),
        active_slots,
        sparse_scoring,
    );
    if plans.is_empty()
        && previous_manifest
            .as_ref()
            .is_some_and(|manifest| !manifest.slots.is_empty())
        && active_slots.is_none_or(|slots| !slots.is_empty())
    {
        return Err(stale(
            "base CF scan produced no searchable slots but the previous search manifest was non-empty; refusing to replace it with an empty manifest",
        ));
    }
    // Validate every staged recovery record before staking a new rebuild
    // marker or writing any derived artifact. A record from an older pin is
    // still crash evidence: silently skipping it here and deleting it during
    // post-publish pruning would turn corruption into apparent recovery.
    let staged_records = validate_present_staged_artifacts(vault_dir, &root, &plans)?;
    progress(RebuildProgress {
        rows: Some(staged_records),
        base_seq: Some(base_seq),
        ..RebuildProgress::phase("staged_preflight_ok")
    })?;
    // Stake the write-ahead rebuild intent after the read-only preflight and
    // before creating any derived index artifact.
    match marker::read_rebuild_required_marker(vault_dir, panel_version)? {
        Some(_) => progress(RebuildProgress::phase("rebuild_marker_preserved"))?,
        None => {
            let mut intent = marker::RebuildRequiredMarker::new(
                panel_version,
                "search_index_rebuild",
                "panel-scoped search-index rebuild in progress; derived indexes are unproven until the manifest is republished",
            )?;
            intent.required_base_seq = Some(base_seq);
            marker::write_rebuild_required_marker(vault_dir, &intent)?;
            progress(RebuildProgress::phase("rebuild_marker_written"))?;
        }
    }
    let parallelism = bounded_parallel_slot_count(&plans)?;
    progress(RebuildProgress {
        rows: Some(plans.len()),
        base_seq: Some(base_seq),
        ..RebuildProgress::phase("slot_plan_ok")
    })?;

    let mut entries = Vec::new();
    let mut total_rows = 0usize;
    for chunk in plans.chunks(parallelism) {
        for plan in chunk {
            progress(RebuildProgress::slot(
                "slot_build_start",
                panel_version,
                plan.slot,
                Some(plan.expected_ids.len()),
                Some(base_seq),
            ))?;
        }
        let progress_lock = Arc::new(Mutex::new(&mut *progress));
        let mut built = chunk
            .par_iter()
            .map(|plan| {
                build_slot_entry(
                    vault_dir,
                    &root,
                    vault,
                    snapshot,
                    plan,
                    page_rows,
                    build_policy,
                    dense_index_config,
                    Some(&progress_lock),
                )
            })
            .collect::<CliResult<Vec<_>>>()?;
        drop(progress_lock);
        built.sort_by_key(|built| built.entry.slot());
        for built in built {
            total_rows += built.row_count;
            progress(RebuildProgress::slot(
                built.ok_phase(),
                panel_version,
                SlotId::new(built.entry.slot()),
                Some(built.row_count),
                Some(base_seq),
            ))?;
            if let Some(entry) = built.entry.into_entry() {
                entries.push(entry);
            }
        }
    }
    entries.sort_by_key(|entry| entry.slot);

    let filter = match reuse_staged_filter_entry(vault_dir, &root, base_seq)? {
        Some(entry) => {
            if entry.len != base_docs.len() {
                return Err(stale(format!(
                    "staged filter artifact at base seq {base_seq} contains {} rows, but the pinned Base snapshot contains {}; refusing to reuse incomplete recovery state",
                    entry.len,
                    base_docs.len()
                )));
            }
            progress(RebuildProgress {
                rows: Some(entry.len),
                base_seq: Some(base_seq),
                ..RebuildProgress::phase("filter_reuse_ok")
            })?;
            entry
        }
        None => {
            progress(RebuildProgress {
                rows: Some(base_docs.len()),
                base_seq: Some(base_seq),
                ..RebuildProgress::phase("filter_start")
            })?;
            let entry = filter::write(vault_dir, &root, &base_docs.docs, base_seq)?;
            write_staged_filter_artifact(&root, base_seq, &entry)?;
            progress(RebuildProgress {
                rows: Some(base_docs.len()),
                base_seq: Some(base_seq),
                ..RebuildProgress::phase("filter_ok")
            })?;
            entry
        }
    };

    let (backend, backend_source, cuvs_compiled) = manifest_backend(build_policy);
    let manifest = SearchIndexManifest {
        format: MANIFEST_FORMAT.to_string(),
        panel_version,
        base_seq,
        diskann_build_backend: Some(backend),
        diskann_build_backend_source: Some(backend_source),
        sextant_cuvs_compiled: Some(cuvs_compiled),
        dense_index_config,
        filter: Some(filter),
        slots: entries,
    };
    progress(RebuildProgress {
        rows: Some(manifest.slots.len()),
        base_seq: Some(base_seq),
        ..RebuildProgress::phase("manifest_validate_start")
    })?;
    validate_staged_manifest_artifacts(vault_dir, &manifest)?;
    progress(RebuildProgress {
        rows: Some(manifest.slots.len()),
        base_seq: Some(base_seq),
        ..RebuildProgress::phase("manifest_validate_ok")
    })?;
    let manifest_path = manifest_path(vault_dir, panel_version);
    progress(RebuildProgress::manifest(
        "manifest_write_start",
        &manifest_path,
        base_seq,
    ))?;
    // Durable (dir-fsynced) so the manifest publish is persisted strictly
    // before the marker removal below can be — otherwise a power loss could
    // surface "marker gone, manifest still old", the exact silent-stale state
    // this machinery exists to prevent.
    write_json_atomic_durable(&manifest_path, &manifest)?;
    progress(RebuildProgress::manifest(
        "manifest_write_ok",
        &manifest_path,
        base_seq,
    ))?;
    progress(RebuildProgress::phase("prune_start"))?;
    prune_stale_index_artifacts(vault_dir, &root, &manifest)?;
    progress(RebuildProgress::phase("prune_ok"))?;
    let cleared = marker::clear_rebuild_required_marker(vault_dir, panel_version, base_seq)?;
    progress(RebuildProgress::phase(match cleared {
        marker::MarkerClearOutcome::Cleared => "rebuild_marker_cleared",
        marker::MarkerClearOutcome::Absent => "rebuild_marker_absent",
    }))?;
    Ok(RebuildSummary {
        slots: manifest.slots.len(),
        total_rows,
        manifest_path,
    })
}

pub(super) fn validate_staged_manifest_artifacts(
    vault_dir: &Path,
    manifest: &SearchIndexManifest,
) -> CliResult {
    if let Some(filter) = &manifest.filter {
        filter::validate_entry(vault_dir, filter, manifest.base_seq)?;
    }
    for entry in &manifest.slots {
        let slot = SlotId::new(entry.slot);
        match entry.kind.as_str() {
            "diskann" | "flat_dense" => {
                dense::validate_entry(vault_dir, entry, manifest.panel_version, slot)?
            }
            "sparse_inverted" | "sparse_bm25" | "sparse_dot" => {
                sparse::validate_entry(vault_dir, entry, manifest.base_seq, slot)?
            }
            "multi_maxsim" | "multi_maxsim_segments" => {
                multi::validate_entry(vault_dir, entry, manifest.base_seq, slot)?
            }
            other => {
                return Err(stale(format!(
                    "persistent slot {slot} staged index kind {other} is unsupported; rebuild the vault search indexes"
                )));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_slot_entry<C: Clock, F>(
    vault_dir: &Path,
    root: &Path,
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    plan: &SlotBuildPlan,
    page_rows: usize,
    build_policy: DiskAnnBuildPolicy,
    dense_index_config: PersistedDenseIndexConfig,
    progress: Option<&SharedRebuildProgress<'_, F>>,
) -> CliResult<BuiltSlot>
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    let base_seq = snapshot.seq();
    if let Some(built) = reuse_staged_slot_entry(vault_dir, root, plan, base_seq)? {
        if built.row_count != plan.expected_ids.len() {
            return Err(stale(format!(
                "staged slot artifact for panel {} slot {} at base seq {base_seq} contains {} rows, but the pinned Base membership contains {}; refusing to reuse incomplete recovery state",
                plan.panel_version,
                plan.slot,
                built.row_count,
                plan.expected_ids.len()
            )));
        }
        if let Some(progress) = progress {
            emit_shared_progress(
                progress,
                RebuildProgress::slot(
                    "slot_reuse_ok",
                    plan.panel_version,
                    plan.slot,
                    Some(built.row_count),
                    Some(base_seq),
                ),
            )?;
        }
        return Ok(built);
    }
    if let Some(progress) = progress {
        emit_shared_progress(
            progress,
            RebuildProgress::slot(
                "slot_index_write_start",
                plan.panel_version,
                plan.slot,
                Some(plan.expected_ids.len()),
                Some(base_seq),
            ),
        )?;
    }
    let rows =
        collect_or_build_slot_from_cf(vault_dir, root, vault, snapshot, plan, page_rows, progress)?;
    let row_count = rows.len();
    if let Some(progress) = progress {
        emit_shared_progress(
            progress,
            RebuildProgress::slot(
                "slot_rows_loaded",
                plan.panel_version,
                plan.slot,
                Some(row_count),
                Some(base_seq),
            ),
        )?;
    }
    let entry = match rows {
        ScannedSlotRows::Dense(rows) => OptionalSearchIndexEntry::Some(dense::write_with_progress(
            vault_dir,
            root,
            PanelSlotId::new(plan.panel_version, plan.slot),
            rows,
            base_seq,
            build_policy,
            dense_index_config,
            |event| match progress {
                Some(progress) => emit_shared_progress(progress, event),
                None => Ok(()),
            },
        )?),
        ScannedSlotRows::Sparse(rows) => {
            let scoring = plan.sparse_scoring.ok_or_else(|| {
                stale(format!(
                    "slot {} contains sparse rows but the active panel/lens contract declares no sparse scoring mode",
                    plan.slot
                ))
            })?;
            OptionalSearchIndexEntry::Some(sparse::write(
                vault_dir, root, plan.slot, rows, base_seq, scoring,
            )?)
        }
        ScannedSlotRows::MultiEntry(entry) => OptionalSearchIndexEntry::Some(entry),
        ScannedSlotRows::AbsentOnly => OptionalSearchIndexEntry::None {
            slot: plan.slot.get(),
        },
    };
    write_staged_slot_artifact(vault_dir, root, plan.slot, base_seq, &entry)?;
    if let Some(progress) = progress {
        emit_shared_progress(
            progress,
            RebuildProgress::slot(
                "slot_index_write_ok",
                plan.panel_version,
                plan.slot,
                Some(row_count),
                Some(base_seq),
            ),
        )?;
    }
    Ok(BuiltSlot { entry, row_count })
}

#[path = "rebuild_staged.rs"]
mod rebuild_staged;
use rebuild_staged::{
    BuiltSlot, OptionalSearchIndexEntry, reuse_staged_filter_entry, reuse_staged_slot_entry,
    validate_present_staged_artifacts, write_staged_filter_artifact, write_staged_slot_artifact,
};

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
