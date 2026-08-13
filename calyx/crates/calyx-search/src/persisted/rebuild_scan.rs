use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::mvcc::Snapshot;
use calyx_aster::vault::AsterVault;
use calyx_aster::vault::encode::{
    EncodedSlotVectorShape, decode_constellation_base, decode_slot_vector, inspect_slot_vector,
};
use calyx_core::{CalyxError, Clock, Constellation, CxId, SlotId, SlotVector};
use rayon::prelude::*;

use super::super::rebuild::RebuildProgress;
use super::super::rebuild_plan::SlotBuildPlan;
use super::super::{
    CALYX_SEARCH_PANEL_SCOPE_REQUIRED, CliResult, SearchIndexEntry, dense, multi, sparse, stale,
};
use super::{SharedRebuildProgress, emit_shared_progress};

// An encoded multi-vector row may approach the 64 MiB segment ceiling.  Keep
// raw slot values at one row per read so the progress page setting cannot
// multiply that hard memory bound.
const SLOT_POINT_READ_ROWS: usize = 1;

pub(super) fn load_base_docs_at<C: Clock, F>(
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    page_rows: usize,
    requested_panel_version: Option<u32>,
    progress: &mut F,
) -> CliResult<LoadedBaseDocs>
where
    F: FnMut(RebuildProgress<'_>) -> CliResult,
{
    let range = all_rows();
    let mut row_count = 0usize;
    let mut ids_by_slot = BTreeMap::<SlotId, Vec<CxId>>::new();
    let mut observed_panel_versions = BTreeSet::new();
    let mut last_retained_cx_id = None;
    vault.scan_cf_range_pages_snapshot(
        snapshot,
        ColumnFamily::Base,
        &range,
        page_rows,
        |page| {
            let decoded = page
                .into_par_iter()
                .map(|(key, bytes)| decode_base_row(key, bytes))
                .collect::<calyx_core::Result<Vec<_>>>()?;
            for (cx_id, mut cx) in decoded {
                observed_panel_versions.insert(cx.panel_version);
                if requested_panel_version.is_none() && observed_panel_versions.len() > 1 {
                    return Err(CalyxError {
                        code: CALYX_SEARCH_PANEL_SCOPE_REQUIRED,
                        message: format!(
                            "vault snapshot {} contains multiple panel versions {:?}; a bare SlotId rebuild is ambiguous",
                            snapshot.seq(),
                            observed_panel_versions
                        ),
                        remediation: "supply the exact panel version and rebuild only that panel generation",
                    });
                }
                if requested_panel_version
                    .is_some_and(|panel_version| cx.panel_version != panel_version)
                {
                    continue;
                }
                if last_retained_cx_id.is_some_and(|previous| previous >= cx_id) {
                    return Err(stale(format!(
                        "base CF search-rebuild scan is not strictly ordered: prior cx_id {last_retained_cx_id:?}, current {cx_id}"
                    ))
                    .into());
                }
                last_retained_cx_id = Some(cx_id);
                // Base rows contain a second copy of every slot payload. The
                // rebuild plan needs only slot membership, while the index
                // writers deliberately reread authoritative payloads from
                // the physical slot CFs below. Retaining the Base copies for
                // the whole rebuild made memory proportional to the complete
                // corpus embedding payload and OOM-killed county-scale runs.
                for slot in cx.slots.keys() {
                    ids_by_slot.entry(*slot).or_default().push(cx_id);
                }
                cx.slots.clear();
                row_count = row_count
                    .checked_add(1)
                    .ok_or_else(|| stale("base CF row count overflow during search rebuild"))?;
            }
            progress(RebuildProgress {
                rows: Some(row_count),
                base_seq: Some(snapshot.seq()),
                ..RebuildProgress::phase("base_scan_page")
            })?;
            Ok(())
        },
    )?;
    let panel_version = match requested_panel_version {
        Some(panel_version) => panel_version,
        None => observed_panel_versions
            .first()
            .copied()
            .ok_or_else(|| CalyxError {
                code: CALYX_SEARCH_PANEL_SCOPE_REQUIRED,
                message: format!(
                    "vault snapshot {} is empty; no panel version can be inferred for a derived index generation",
                    snapshot.seq()
                ),
                remediation: "supply the exact panel version from the active panel contract",
            })?,
    };
    Ok(LoadedBaseDocs {
        panel_version,
        row_count,
        ids_by_slot,
    })
}

pub(super) struct LoadedBaseDocs {
    panel_version: u32,
    row_count: usize,
    pub(super) ids_by_slot: BTreeMap<SlotId, Vec<CxId>>,
}

impl LoadedBaseDocs {
    pub(super) fn panel_version(&self) -> u32 {
        self.panel_version
    }

    pub(super) fn len(&self) -> usize {
        self.row_count
    }

    pub(super) fn slot_memberships(&self) -> usize {
        self.ids_by_slot.values().map(Vec::len).sum()
    }
}

fn decode_base_row(key: Vec<u8>, bytes: Vec<u8>) -> calyx_core::Result<(CxId, Constellation)> {
    let cx_id = cx_id_from_cf_key(&key, "base CF")?;
    let cx = decode_constellation_base(&bytes)?;
    if cx.cx_id != cx_id {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "base CF key {cx_id} contains constellation {}",
            cx.cx_id
        )));
    }
    Ok((cx_id, cx))
}

pub(super) enum ScannedSlotRows {
    Dense(dense::DenseSlotRows),
    Sparse(sparse::SparseSlotRows),
    MultiEntry(SearchIndexEntry),
    AbsentOnly,
}

impl ScannedSlotRows {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Dense(rows) => rows.len(),
            Self::Sparse(rows) => rows.len(),
            Self::MultiEntry(entry) => entry.len,
            Self::AbsentOnly => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotRowShape {
    Dense,
    Sparse,
    Multi,
}

pub(super) fn collect_or_build_slot_from_cf<C: Clock, F>(
    vault_dir: &Path,
    root: &Path,
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    plan: &SlotBuildPlan,
    page_rows: usize,
    progress: Option<&SharedRebuildProgress<'_, F>>,
) -> CliResult<ScannedSlotRows>
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    let mut found = 0usize;
    let mut shape = None;
    let mut dense_dim = None;
    let mut sparse_dim = None;
    let mut multi_token_dim = None;
    let mut dense_rows = Vec::new();
    let mut sparse_rows = Vec::new();
    let mut multi_writer = None;
    let scan_result: CliResult = (|| {
        for ids in plan.expected_ids.chunks(SLOT_POINT_READ_ROWS) {
            let rows = vault.read_slot_cf_batch_snapshot(snapshot, plan.slot, ids)?;
            if rows.len() != ids.len() {
                return Err(stale(format!(
                    "slot {} batch point read returned {} rows for {} requested IDs",
                    plan.slot,
                    rows.len(),
                    ids.len()
                )));
            }
            for (cx_id, bytes) in ids.iter().copied().zip(rows) {
                let bytes = bytes.ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "slot CF row missing for slot {} cx_id {cx_id}",
                        plan.slot
                    ))
                })?;
                let encoded_shape = inspect_slot_vector(&bytes).map_err(|error| {
                    CalyxError::aster_corrupt_shard(format!(
                        "slot {} cx {cx_id} has malformed encoded payload: {}",
                        plan.slot, error.message
                    ))
                })?;
                let flushed = match encoded_shape {
                    EncodedSlotVectorShape::Multi {
                        token_dim,
                        token_count,
                    } => {
                        multi::ensure_streaming_row_bounded(
                            plan.slot,
                            cx_id,
                            token_dim,
                            token_count,
                            bytes.len(),
                        )?;
                        push_encoded_multi(
                            plan,
                            cx_id,
                            token_dim,
                            token_count,
                            bytes,
                            &mut shape,
                            &mut multi_token_dim,
                            &mut multi_writer,
                            vault_dir,
                            root,
                            snapshot.seq(),
                        )?
                    }
                    EncodedSlotVectorShape::Dense { .. }
                    | EncodedSlotVectorShape::Sparse { .. }
                    | EncodedSlotVectorShape::Absent => push_slot_vector(
                        plan,
                        cx_id,
                        decode_slot_vector(&bytes)?,
                        &mut shape,
                        &mut dense_dim,
                        &mut sparse_dim,
                        &mut multi_token_dim,
                        &mut dense_rows,
                        &mut sparse_rows,
                        &mut multi_writer,
                        vault_dir,
                        root,
                        snapshot.seq(),
                    )?,
                };
                if let Some(flushed) = flushed {
                    emit_segment_flush(progress, plan, snapshot.seq(), flushed)?;
                }
                found += 1;
            }
            if let Some(progress) = progress
                && (found.is_multiple_of(page_rows) || found == plan.expected_ids.len())
            {
                emit_shared_progress(
                    progress,
                    RebuildProgress::slot(
                        "slot_point_read_page",
                        plan.panel_version,
                        plan.slot,
                        Some(found),
                        Some(snapshot.seq()),
                    ),
                )?;
            }
        }
        Ok(())
    })();
    if let Err(primary) = scan_result {
        return Err(abort_multi_writer(&mut multi_writer, primary));
    }
    debug_assert_eq!(found, plan.expected_ids.len());
    match shape {
        Some(SlotRowShape::Dense) => Ok(ScannedSlotRows::Dense(dense::DenseSlotRows {
            dim: dense_dim.expect("dense shape has dim"),
            rows: dense_rows,
        })),
        Some(SlotRowShape::Sparse) => Ok(ScannedSlotRows::Sparse(sparse::SparseSlotRows {
            dim: sparse_dim.expect("sparse shape has dim"),
            rows: sparse_rows,
        })),
        Some(SlotRowShape::Multi) => {
            let writer = multi_writer
                .ok_or_else(|| stale(format!("slot {} has multi shape but no rows", plan.slot)))?;
            let (entry, flushed) = writer.finish()?;
            if let Some(flushed) = flushed {
                emit_segment_flush(progress, plan, snapshot.seq(), flushed)?;
            }
            Ok(ScannedSlotRows::MultiEntry(entry))
        }
        None => Ok(ScannedSlotRows::AbsentOnly),
    }
}

#[allow(clippy::too_many_arguments)]
fn push_slot_vector(
    plan: &SlotBuildPlan,
    cx_id: CxId,
    vector: SlotVector,
    shape: &mut Option<SlotRowShape>,
    dense_dim: &mut Option<u32>,
    sparse_dim: &mut Option<u32>,
    multi_token_dim: &mut Option<u32>,
    dense_rows: &mut Vec<(CxId, Vec<f32>)>,
    sparse_rows: &mut Vec<(CxId, Vec<calyx_core::SparseEntry>)>,
    multi_writer: &mut Option<multi::StreamingSegmentsWriter>,
    vault_dir: &Path,
    root: &Path,
    base_seq: u64,
) -> CliResult<Option<multi::SegmentFlush>> {
    vector.validate_schema().map_err(|err| {
        stale(format!(
            "slot {} cx {cx_id} has invalid payload: {}",
            plan.slot, err.message
        ))
    })?;
    match vector {
        SlotVector::Dense { dim, data } => {
            require_shape(shape, SlotRowShape::Dense, plan.slot, cx_id)?;
            dense::validate_dense(plan.slot, cx_id, dim, &data)?;
            match *dense_dim {
                Some(expected_dim) if expected_dim != dim => {
                    return Err(stale(format!(
                        "slot {} has mixed dense dims: {expected_dim} and {dim}",
                        plan.slot
                    )));
                }
                None => *dense_dim = Some(dim),
                _ => {}
            }
            dense_rows.push((cx_id, data));
            Ok(None)
        }
        SlotVector::Sparse { dim, entries } => {
            require_shape(shape, SlotRowShape::Sparse, plan.slot, cx_id)?;
            match *sparse_dim {
                Some(expected_dim) if expected_dim != dim => {
                    return Err(stale(format!(
                        "slot {} has mixed sparse dims: {expected_dim} and {dim}",
                        plan.slot
                    )));
                }
                None => *sparse_dim = Some(dim),
                _ => {}
            }
            sparse_rows.push((cx_id, entries));
            Ok(None)
        }
        SlotVector::Multi { token_dim, tokens } => {
            require_shape(shape, SlotRowShape::Multi, plan.slot, cx_id)?;
            match *multi_token_dim {
                Some(expected_dim) if expected_dim != token_dim => {
                    return Err(stale(format!(
                        "slot {} has mixed multi token dims: {expected_dim} and {token_dim}",
                        plan.slot
                    )));
                }
                None => *multi_token_dim = Some(token_dim),
                _ => {}
            }
            if multi_writer.is_none() {
                *multi_writer = Some(multi::StreamingSegmentsWriter::new(
                    vault_dir, root, plan.slot, token_dim, base_seq,
                ));
            }
            multi_writer
                .as_mut()
                .expect("multi writer initialized")
                .push(cx_id, tokens)
        }
        SlotVector::Absent { .. } => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn push_encoded_multi(
    plan: &SlotBuildPlan,
    cx_id: CxId,
    token_dim: u32,
    token_count: u32,
    bytes: Vec<u8>,
    shape: &mut Option<SlotRowShape>,
    multi_token_dim: &mut Option<u32>,
    multi_writer: &mut Option<multi::StreamingSegmentsWriter>,
    vault_dir: &Path,
    root: &Path,
    base_seq: u64,
) -> CliResult<Option<multi::SegmentFlush>> {
    require_shape(shape, SlotRowShape::Multi, plan.slot, cx_id)?;
    match *multi_token_dim {
        Some(expected_dim) if expected_dim != token_dim => {
            return Err(stale(format!(
                "slot {} has mixed multi token dims: {expected_dim} and {token_dim}",
                plan.slot
            )));
        }
        None => *multi_token_dim = Some(token_dim),
        _ => {}
    }
    if multi_writer.is_none() {
        *multi_writer = Some(multi::StreamingSegmentsWriter::new(
            vault_dir, root, plan.slot, token_dim, base_seq,
        ));
    }
    multi_writer
        .as_mut()
        .expect("multi writer initialized")
        .push_encoded(cx_id, token_count, bytes)
}

fn emit_segment_flush<F>(
    progress: Option<&SharedRebuildProgress<'_, F>>,
    plan: &SlotBuildPlan,
    base_seq: u64,
    flushed: multi::SegmentFlush,
) -> CliResult
where
    F: FnMut(RebuildProgress<'_>) -> CliResult + Send,
{
    if let Some(progress) = progress {
        emit_shared_progress(
            progress,
            RebuildProgress {
                detail: Some(format!(
                    "{} segment_rows={} segment_ordinal={}",
                    flushed.detail, flushed.row_count, flushed.ordinal
                )),
                ..RebuildProgress::slot(
                    "multi_segment_write_ok",
                    plan.panel_version,
                    plan.slot,
                    Some(flushed.total_rows),
                    Some(base_seq),
                )
            },
        )?;
    }
    Ok(())
}

fn abort_multi_writer(
    writer: &mut Option<multi::StreamingSegmentsWriter>,
    primary: crate::error::CliError,
) -> crate::error::CliError {
    let Some(writer) = writer.take() else {
        return primary;
    };
    match writer.abort() {
        Ok(()) => primary,
        Err(cleanup) => stale(format!(
            "slot scan failed [{}] {}; partial multi segment cleanup also failed [{}] {}",
            primary.code(),
            primary.message(),
            cleanup.code(),
            cleanup.message()
        )),
    }
}

fn require_shape(
    current: &mut Option<SlotRowShape>,
    next: SlotRowShape,
    slot: SlotId,
    cx_id: CxId,
) -> CliResult {
    match current {
        Some(existing) if *existing != next => Err(stale(format!(
            "slot {slot} mixes {existing:?} rows with {next:?} row at cx {cx_id}; reingest/backfill the vault"
        ))),
        Some(_) => Ok(()),
        None => {
            *current = Some(next);
            Ok(())
        }
    }
}

fn cx_id_from_cf_key(key: &[u8], cf_name: &str) -> calyx_core::Result<CxId> {
    let bytes: [u8; 16] = key.try_into().map_err(|_| {
        CalyxError::vault_access_denied(format!("{cf_name} key has {} bytes", key.len()))
    })?;
    Ok(CxId::from_bytes(bytes))
}

fn all_rows() -> KeyRange {
    KeyRange {
        start: Vec::new(),
        end: None,
    }
}
