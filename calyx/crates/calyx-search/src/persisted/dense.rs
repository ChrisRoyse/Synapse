use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use calyx_core::{CalyxError, CxId, PanelSlotId, SlotId, SlotVector};
use calyx_sextant::index::{
    DiskAnnBuildParams, DiskAnnSearch, DiskAnnSearchParams, IndexSearchHit, SextantIndex, ranked,
};

use super::rebuild::RebuildProgress;
use super::rebuild_plan::DiskAnnBuildPolicy;
use super::{SearchIndexEntry, SlotIdMap, rel, stale, write_json_atomic};
use crate::error::CliResult;

#[path = "dense/flat.rs"]
mod flat;

#[derive(Clone, Debug)]
pub(super) struct DenseSlotRows {
    pub(super) dim: u32,
    pub(super) rows: Vec<(CxId, Vec<f32>)>,
}

impl DenseSlotRows {
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
}

pub(super) fn write_with_progress<F>(
    vault_dir: &Path,
    root: &Path,
    panel_slot: PanelSlotId,
    rows: DenseSlotRows,
    base_seq: u64,
    build_policy: DiskAnnBuildPolicy,
    mut progress: F,
) -> CliResult<SearchIndexEntry>
where
    F: FnMut(RebuildProgress<'_>) -> CliResult,
{
    let slot = panel_slot.slot_id();
    let panel_version = panel_slot.panel_version();
    if should_use_flat_dense_index(rows.rows.len()) {
        return flat::write(vault_dir, root, slot, rows, base_seq);
    }
    let dir_name = format!(
        "slot_{:05}_seq_{:020}_n_{:010}.ann",
        slot.get(),
        base_seq,
        rows.rows.len()
    );
    let dir = root.join(&dir_name);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    let graph_path = dir.join("graph.cda");
    DiskAnnSearch::build_with_backend_and_progress(
        slot,
        &graph_path,
        &rows.rows,
        build_params(rows.dim as usize),
        None,
        search_params(rows.rows.len().max(64)),
        build_policy.backend,
        |event| {
            progress(RebuildProgress::slot(
                event.phase,
                panel_version,
                slot,
                Some(event.rows),
                Some(base_seq),
            ))
            .map_err(CalyxError::from)
        },
    )?;
    let id_map_path = dir.join("ids.json");
    write_json_atomic(
        &id_map_path,
        &SlotIdMap {
            format: super::IDMAP_FORMAT.to_string(),
            panel_version,
            slot: slot.get(),
            ids: rows.rows.iter().map(|(cx_id, _)| *cx_id).collect(),
        },
    )?;
    Ok(SearchIndexEntry::dense(
        slot,
        rows.dim,
        rows.rows.len(),
        base_seq,
        rel(vault_dir, &graph_path)?,
        rel(vault_dir, &id_map_path)?,
    ))
}

pub(super) fn search(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
) -> CliResult<Vec<IndexSearchHit>> {
    if entry.kind == "flat_dense" {
        return flat::search(vault_dir, entry, slot, query, k, None);
    }
    let SlotVector::Dense { dim, .. } = query else {
        return Err(stale(format!(
            "persistent dense search slot {slot} received non-dense query"
        )));
    };
    open(vault_dir, entry, panel_version, slot, *dim, k)?
        .search(query, want(k, entry.len), Some(want(k, entry.len).max(64)))
        .map_err(Into::into)
}

pub(super) fn search_filtered(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
    candidates: &BTreeSet<CxId>,
) -> CliResult<Vec<IndexSearchHit>> {
    if entry.kind == "flat_dense" {
        return flat::search(vault_dir, entry, slot, query, k, Some(candidates));
    }
    let SlotVector::Dense { dim, data } = query else {
        return Err(stale(format!(
            "persistent dense filtered search slot {slot} received non-dense query"
        )));
    };
    let index = open(vault_dir, entry, panel_version, slot, *dim, k)?;
    exact_filtered_hits(&index, data, k, candidates)
}

fn open(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    query_dim: u32,
    k: usize,
) -> CliResult<DiskAnnSearch> {
    entry.require_kind("diskann", slot)?;
    let dim = entry.require_dim(slot)?;
    if dim != query_dim {
        return Err(stale(format!(
            "persistent slot {slot} index dim {dim} != query dim {query_dim}; reingest/backfill the vault"
        )));
    }
    let ids = read_ids(vault_dir, entry, panel_version, slot)?;
    if ids.len() != entry.len {
        return Err(stale(format!(
            "persistent slot {slot} id map len {} != manifest len {}",
            ids.len(),
            entry.len
        )));
    }
    let graph = vault_dir.join(entry.require_graph_rel(slot)?);
    let mut index = DiskAnnSearch::open(slot, graph, ids, None, search_params(k.max(64)))?;
    index.set_base_seq(entry.built_at_seq);
    Ok(index)
}

fn read_ids(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
) -> CliResult<Vec<CxId>> {
    let path = vault_dir.join(entry.require_id_map_rel(slot)?);
    let map: SlotIdMap = serde_json::from_slice(&fs::read(&path)?)?;
    if map.format != super::IDMAP_FORMAT {
        return Err(stale(format!(
            "persistent slot {slot} id map {} has format {}; expected {}",
            path.display(),
            map.format,
            super::IDMAP_FORMAT
        )));
    }
    if map.slot != entry.slot {
        return Err(stale(format!(
            "persistent id map slot {} != manifest slot {}",
            map.slot, entry.slot
        )));
    }
    if map.panel_version != panel_version {
        return Err(stale(format!(
            "persistent id map panel {} != requested panel {panel_version} for slot {slot}",
            map.panel_version
        )));
    }
    Ok(map.ids)
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
) -> CliResult {
    if entry.kind == "flat_dense" {
        return flat::validate_entry(vault_dir, entry, slot);
    }
    entry.require_kind("diskann", slot)?;
    let graph = vault_dir.join(entry.require_graph_rel(slot)?);
    if !graph.is_file() {
        return Err(stale(format!(
            "persistent slot {slot} graph sidecar missing at {}; rebuild the vault search indexes",
            graph.display()
        )));
    }
    let graph_len = fs::metadata(&graph)?.len();
    if graph_len == 0 {
        return Err(stale(format!(
            "persistent slot {slot} graph sidecar {} is empty; rebuild the vault search indexes",
            graph.display()
        )));
    }
    let ids = read_ids(vault_dir, entry, panel_version, slot)?;
    if ids.len() != entry.len {
        return Err(stale(format!(
            "persistent slot {slot} id map len {} != manifest len {}",
            ids.len(),
            entry.len
        )));
    }
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(stale(format!(
                "persistent slot {slot} id map repeats {id}; rebuild the vault search indexes"
            )));
        }
    }
    Ok(())
}

fn exact_filtered_hits(
    index: &DiskAnnSearch,
    query: &[f32],
    k: usize,
    candidates: &BTreeSet<CxId>,
) -> CliResult<Vec<IndexSearchHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let mut scored = Vec::new();
    for cx_id in candidates {
        let Some(vector) = index.vector(*cx_id) else {
            continue;
        };
        let Some(values) = vector.as_dense() else {
            return Err(stale(format!(
                "persistent filtered search candidate {cx_id} is not dense; rebuild the vault search indexes"
            )));
        };
        if values.len() != query.len() {
            return Err(stale(format!(
                "persistent filtered search candidate {cx_id} dim {} != query dim {}; rebuild the vault search indexes",
                values.len(),
                query.len()
            )));
        }
        scored.push((*cx_id, cosine(query, values)));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
    });
    scored.truncate(k);
    Ok(ranked(scored))
}

pub(super) fn should_use_flat_dense_index(row_count: usize) -> bool {
    flat::should_use_index(row_count)
}

pub(super) fn score_replacements(
    slot: SlotId,
    query_vector: &SlotVector,
    replacements: &BTreeMap<CxId, SlotVector>,
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<(CxId, f32)>> {
    query_vector.validate_schema().map_err(|error| {
        stale(format!(
            "dense delta reconciliation query for slot {slot} is invalid: {error}"
        ))
    })?;
    let SlotVector::Dense {
        dim: query_dim,
        data: query,
    } = query_vector
    else {
        return Err(stale(format!(
            "dense delta reconciliation for slot {slot} received non-dense query"
        )));
    };
    replacements
        .iter()
        .filter(|(cx_id, _)| candidates.is_none_or(|allowed| allowed.contains(cx_id)))
        .map(|(cx_id, vector)| {
            let SlotVector::Dense { dim, data } = vector else {
                return Err(stale(format!(
                    "changed row {cx_id} for dense slot {slot} has a non-dense vector"
                )));
            };
            validate_dense(slot, *cx_id, *dim, data)?;
            if dim != query_dim {
                return Err(stale(format!(
                    "changed row {cx_id} for dense slot {slot} has dim {dim}, expected query dim {query_dim}"
                )));
            }
            Ok((*cx_id, cosine(query, data)))
        })
        .collect()
}

pub(super) fn validate_dense(slot: SlotId, cx_id: CxId, dim: u32, data: &[f32]) -> CliResult {
    if dim == 0 || data.len() != dim as usize {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "slot {slot} cx {cx_id} dense len {} != dim {dim}",
            data.len()
        ))
        .into());
    }
    if data.iter().any(|value| !value.is_finite()) {
        return Err(CalyxError::lens_numerical_invariant(format!(
            "slot {slot} cx {cx_id} has non-finite dense component"
        ))
        .into());
    }
    Ok(())
}

fn build_params(dim: usize) -> DiskAnnBuildParams {
    DiskAnnBuildParams {
        dim,
        m_max: 32,
        ef_construction: 64,
        alpha: 1.2,
    }
}

fn search_params(ef: usize) -> DiskAnnSearchParams {
    DiskAnnSearchParams {
        beamwidth: 32,
        ef_search: ef,
        rescore_k: ef,
        rescore_from_raw: false,
    }
}

fn want(k: usize, len: usize) -> usize {
    k.max(1).min(len)
}

/// Similarity for the persisted dense lanes, scored through Sextant's
/// runtime-dispatched kernel rather than a private scalar loop.
///
/// # Why this is not hand-written here any more
///
/// This was a plain `for` loop accumulating `dot`/`left_l2`/`right_l2`. A float
/// reduction carries a serial dependency, and Rust does not enable fast-math, so
/// LLVM may not reassociate it — the loop therefore stays scalar no matter what
/// `target-cpu` the binary is built at. It was the only dense scoring path in
/// the workspace without SIMD dispatch: `index/distance.rs` has selected an AVX2
/// kernel by `is_x86_feature_detected!` since it was written, and DiskANN has
/// always scored through it.
///
/// Measured on this workspace's default `target-cpu=x86-64-v2` baseline
/// (`synapse-storage --example host_math_baseline_fsv`), 20k rows per pass:
///
/// ```text
///   dim=32    private scalar loop  29.0M rows/s   ->  dispatched  59.3M rows/s
///   dim=384   private scalar loop   2.76M rows/s  ->  dispatched   6.54M rows/s
/// ```
///
/// The dispatch is a runtime feature check, so this is not a portability trade:
/// a host without AVX2 gets the scalar kernel and the previous throughput, and a
/// host with it gets roughly double without the binary having to be rebuilt for
/// that host. Raising the compile baseline was measured as the alternative and
/// moved this same path only ~7-12% — within run-to-run variance on a laptop —
/// because of the reassociation limit above.
///
/// `cosine_distance` returns `1 - cos`, so similarity is `1 - distance`. It
/// clamps the distance at 0, which bounds similarity at 1.0 and removes the
/// slightly-above-1.0 values float error used to allow here; a zero-norm vector
/// yields distance 1.0, i.e. similarity 0.0, exactly as before. Scores differ
/// from the old loop only in the last few significant digits, from AVX2's
/// different summation order.
fn cosine(left: &[f32], right: &[f32]) -> f32 {
    1.0 - calyx_sextant::index::distance::cosine_distance(left, right)
}
