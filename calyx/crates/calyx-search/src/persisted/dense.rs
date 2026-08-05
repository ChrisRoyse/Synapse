use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use calyx_core::{CalyxError, CxId, PanelSlotId, SlotId, SlotVector};
use calyx_sextant::index::{
    DiskAnnBuildParams, DiskAnnPqBuildParams, DiskAnnPqSearchBuild, DiskAnnSearch,
    DiskAnnSearchParams, IndexSearchHit, SextantIndex, ranked,
};

use super::rebuild::RebuildProgress;
use super::rebuild_plan::DiskAnnBuildPolicy;
use super::{
    DenseQuantizationEntry, PersistedDenseIndexConfig, SearchIndexEntry, SlotIdMap, rel,
    sha256_hex, stale, write_json_atomic,
};
use crate::error::CliResult;

#[path = "dense/flat.rs"]
mod flat;

#[derive(Clone, Debug)]
pub(super) struct DenseSlotRows {
    pub(super) dim: u32,
    pub(super) rows: Vec<(CxId, Vec<f32>)>,
}

#[derive(Clone)]
pub(super) struct DenseBuildOptions {
    pub(super) base_seq: u64,
    pub(super) policy: DiskAnnBuildPolicy,
    pub(super) config: PersistedDenseIndexConfig,
}

#[derive(Clone)]
pub(super) struct DenseSearchContext {
    pub(super) panel_version: u32,
    pub(super) slot: SlotId,
    pub(super) config: PersistedDenseIndexConfig,
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
    options: DenseBuildOptions,
    mut progress: F,
) -> CliResult<SearchIndexEntry>
where
    F: FnMut(RebuildProgress<'_>) -> CliResult,
{
    let DenseBuildOptions {
        base_seq,
        policy: build_policy,
        config,
    } = options;
    let slot = panel_slot.slot_id();
    let panel_version = panel_slot.panel_version();
    let quant_bits = config.quant_bits_for(slot);
    if quant_bits == 32 && should_use_flat_dense_index(rows.rows.len()) {
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
    let mut quantization = None;
    if quant_bits == 32 {
        DiskAnnSearch::build_with_backend_and_progress(
            slot,
            &graph_path,
            &rows.rows,
            build_params(rows.dim as usize, &config),
            None,
            search_params(&config, quant_bits),
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
    } else {
        progress(RebuildProgress::slot(
            "slot.dense.quantized.start",
            panel_version,
            slot,
            Some(rows.rows.len()),
            Some(base_seq),
        ))?;
        let params = pq_params(rows.dim as usize, quant_bits)?;
        DiskAnnSearch::build_with_pq_plan(
            slot,
            &graph_path,
            &rows.rows,
            build_params(rows.dim as usize, &config),
            None,
            DiskAnnPqSearchBuild {
                search: search_params(&config, quant_bits),
                pq: params,
                backend: build_policy.backend,
            },
        )?;
        progress(RebuildProgress::slot(
            "slot.dense.quantized.done",
            panel_version,
            slot,
            Some(rows.rows.len()),
            Some(base_seq),
        ))?;
        let pq_path = graph_path.with_extension("pq");
        let raw_path = graph_path.with_extension("raw");
        let pq_bytes = fs::read(&pq_path)?;
        let raw_bytes = fs::read(&raw_path)?;
        quantization = Some(DenseQuantizationEntry {
            bits: quant_bits,
            subvectors: params.subvectors,
            centroids: params.centroids.min(rows.rows.len()),
            pq_rel: rel(vault_dir, &pq_path)?,
            pq_sha256: sha256_hex(&pq_bytes),
            raw_rel: rel(vault_dir, &raw_path)?,
            raw_sha256: sha256_hex(&raw_bytes),
        });
    }
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
        quantization,
    ))
}

pub(super) fn search(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
    config: PersistedDenseIndexConfig,
) -> CliResult<Vec<IndexSearchHit>> {
    if entry.kind == "flat_dense" {
        return flat::search(vault_dir, entry, slot, query, k, None);
    }
    let SlotVector::Dense { dim, .. } = query else {
        return Err(stale(format!(
            "persistent dense search slot {slot} received non-dense query"
        )));
    };
    open(vault_dir, entry, panel_version, slot, *dim, config.clone())?
        .search(query, want(k, entry.len), Some(config.ef_search.max(k)))
        .map_err(Into::into)
}

pub(super) fn ids(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
) -> CliResult<Vec<CxId>> {
    if entry.kind != "diskann" && entry.kind != "flat_dense" {
        return Err(stale(format!(
            "persistent slot {slot} is {}, not a dense lane",
            entry.kind
        )));
    }
    if entry.kind == "flat_dense" {
        return flat::ids(vault_dir, entry, slot);
    }
    read_ids(vault_dir, entry, panel_version, slot)
}

pub(super) fn exact_search(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
    config: PersistedDenseIndexConfig,
) -> CliResult<Vec<IndexSearchHit>> {
    if entry.kind == "flat_dense" {
        return flat::search(vault_dir, entry, slot, query, k, None);
    }
    let SlotVector::Dense { dim, data } = query else {
        return Err(stale(format!(
            "persistent exact dense search slot {slot} received non-dense query"
        )));
    };
    let index = open(vault_dir, entry, panel_version, slot, *dim, config)?;
    let candidates = read_ids(vault_dir, entry, panel_version, slot)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    exact_filtered_hits(&index, data, k, &candidates)
}

pub(super) fn search_filtered(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    context: DenseSearchContext,
    query: &SlotVector,
    k: usize,
    candidates: &BTreeSet<CxId>,
) -> CliResult<Vec<IndexSearchHit>> {
    let DenseSearchContext {
        panel_version,
        slot,
        config,
    } = context;
    if entry.kind == "flat_dense" {
        return flat::search(vault_dir, entry, slot, query, k, Some(candidates));
    }
    let SlotVector::Dense { dim, data } = query else {
        return Err(stale(format!(
            "persistent dense filtered search slot {slot} received non-dense query"
        )));
    };
    let index = open(vault_dir, entry, panel_version, slot, *dim, config)?;
    exact_filtered_hits(&index, data, k, candidates)
}

fn open(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    panel_version: u32,
    slot: SlotId,
    query_dim: u32,
    config: PersistedDenseIndexConfig,
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
    let quant_bits = config.quant_bits_for(slot);
    validate_quantization_artifacts(vault_dir, entry, slot, quant_bits)?;
    let mut index =
        DiskAnnSearch::open(slot, graph, ids, None, search_params(&config, quant_bits))?;
    index.set_base_seq(entry.built_at_seq);
    Ok(index)
}

fn validate_quantization_artifacts(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
    quant_bits: u8,
) -> CliResult {
    let Some(quantization) = &entry.dense_quantization else {
        if quant_bits == 32 {
            return Ok(());
        }
        return Err(stale(format!(
            "persistent slot {slot} config requires {}-bit quantization but the manifest has no quantization artifacts",
            quant_bits
        )));
    };
    if quantization.bits != quant_bits || quant_bits == 32 {
        return Err(stale(format!(
            "persistent slot {slot} quantization bits {} disagree with generation config {}",
            quantization.bits, quant_bits
        )));
    }
    for (kind, rel_path, expected_hash) in [
        ("pq", &quantization.pq_rel, &quantization.pq_sha256),
        ("raw", &quantization.raw_rel, &quantization.raw_sha256),
    ] {
        let path = vault_dir.join(rel_path);
        let bytes = fs::read(&path).map_err(|error| {
            stale(format!(
                "persistent slot {slot} {kind} artifact {} cannot be read: {error}",
                path.display()
            ))
        })?;
        let observed = sha256_hex(&bytes);
        if &observed != expected_hash {
            return Err(stale(format!(
                "persistent slot {slot} {kind} artifact {} sha256 {observed} != manifest {expected_hash}",
                path.display()
            )));
        }
    }
    Ok(())
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

fn build_params(dim: usize, config: &PersistedDenseIndexConfig) -> DiskAnnBuildParams {
    DiskAnnBuildParams {
        dim,
        m_max: config.m_max,
        ef_construction: config.ef_construction,
        alpha: config.alpha,
    }
}

fn search_params(config: &PersistedDenseIndexConfig, quant_bits: u8) -> DiskAnnSearchParams {
    DiskAnnSearchParams {
        beamwidth: config.beamwidth,
        ef_search: config.ef_search,
        rescore_k: config.ef_search,
        rescore_from_raw: quant_bits != 32,
    }
}

fn pq_params(dim: usize, quant_bits: u8) -> CliResult<DiskAnnPqBuildParams> {
    let subvectors = (1..=dim.min(16))
        .rev()
        .find(|candidate| dim.is_multiple_of(*candidate))
        .ok_or_else(|| {
            stale(format!(
                "dense dimension {dim} has no valid PQ subvector count"
            ))
        })?;
    Ok(DiskAnnPqBuildParams {
        subvectors,
        centroids: 1_usize << quant_bits,
        iterations: 8,
    })
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
