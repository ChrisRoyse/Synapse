use std::fs;
use std::path::Path;

use calyx_aster::cf::full_content_hash;
use calyx_core::{CalyxError, Result};
use serde::Serialize;

use crate::{ArtifactPtr, BudgetHandle, CALYX_ANNEAL_BUDGET_EXHAUSTED};

use super::{CALYX_ANNEAL_REBUILD_IO, MvccSnapshot, RebuildTarget};

pub(super) type RawSourceRows = Vec<(Vec<u8>, Vec<u8>)>;
type SourceRowSetInput = (&'static str, RawSourceRows);

#[derive(Serialize)]
pub(super) struct ArtifactRowSet<'a> {
    cf: &'a str,
    rows: Vec<ArtifactRow>,
}

#[derive(Serialize)]
struct ArtifactRow {
    key_hex: String,
    value_len: usize,
    value_hash: String,
}

pub(super) fn source_rows(
    sets: Vec<SourceRowSetInput>,
    budget: &mut BudgetHandle,
) -> Result<Vec<ArtifactRowSet<'static>>> {
    let mut out = Vec::with_capacity(sets.len());
    for (cf, mut rows) in sets {
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        let mut encoded = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            if !budget.try_consume() {
                return Err(budget_exhausted());
            }
            encoded.push(ArtifactRow {
                key_hex: hex(&key),
                value_len: value.len(),
                value_hash: hex(&full_content_hash([value.as_slice()])),
            });
        }
        out.push(ArtifactRowSet { cf, rows: encoded });
    }
    Ok(out)
}

pub(super) fn artifact_hash(
    tag: &'static str,
    target: &RebuildTarget,
    snapshot: MvccSnapshot,
    rows: &[ArtifactRowSet<'_>],
) -> Result<[u8; 32]> {
    artifact_bytes(tag, target, snapshot, rows).map(|bytes| full_content_hash([bytes.as_slice()]))
}

pub(super) fn artifact_bytes(
    tag: &'static str,
    target: &RebuildTarget,
    snapshot: MvccSnapshot,
    rows: &[ArtifactRowSet<'_>],
) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Artifact<'a> {
        tag: &'static str,
        target: &'a RebuildTarget,
        snapshot: MvccSnapshot,
        rows: &'a [ArtifactRowSet<'a>],
    }
    serde_json::to_vec_pretty(&Artifact {
        tag,
        target,
        snapshot,
        rows,
    })
    .map_err(|error| io_error(format!("encode rebuilt artifact: {error}")))
}

pub(super) fn write_artifact(
    dir: &Path,
    prefix: &str,
    target: &RebuildTarget,
    bytes: &[u8],
) -> Result<String> {
    fs::create_dir_all(dir)
        .map_err(|error| io_error(format!("create {}: {error}", dir.display())))?;
    let digest = full_content_hash([bytes]);
    let path = dir.join(format!("{prefix}-{}.json", hex(&digest)));
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        hex(&target_hash(target)),
        hex(&digest)
    ));
    fs::write(&tmp, bytes)
        .map_err(|error| io_error(format!("write {}: {error}", tmp.display())))?;
    fs::rename(&tmp, &path)
        .map_err(|error| io_error(format!("rename {}: {error}", path.display())))?;
    Ok(path.to_string_lossy().into_owned())
}

pub(super) fn target_hash(target: &RebuildTarget) -> [u8; 32] {
    let mut bytes = Vec::new();
    match target {
        RebuildTarget::AnnIndex { panel_slot } => {
            bytes.extend_from_slice(b"ann_index\0");
            bytes.extend_from_slice(&panel_slot.panel_version().to_be_bytes());
            bytes.extend_from_slice(&panel_slot.slot_id().get().to_be_bytes());
        }
        RebuildTarget::KernelIndex { scope } => {
            bytes.extend_from_slice(b"kernel_index\0");
            bytes.extend_from_slice(scope.to_string().as_bytes());
        }
        RebuildTarget::GuardProfile { panel_slot } => {
            bytes.extend_from_slice(b"guard_profile\0");
            bytes.extend_from_slice(&panel_slot.panel_version().to_be_bytes());
            bytes.extend_from_slice(&panel_slot.slot_id().get().to_be_bytes());
        }
    }
    full_content_hash([bytes.as_slice()])
}

pub(super) fn ptr_hash(ptr: &ArtifactPtr) -> [u8; 32] {
    match ptr {
        ArtifactPtr::ConfigCacheKeyHash(hash) | ArtifactPtr::QuantLevelRecordHash(hash) => *hash,
        ArtifactPtr::HnswGraphPath(path) => full_content_hash([path.as_bytes()]),
    }
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn io_error(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_REBUILD_IO,
        message: message.into(),
        remediation: "repair the rebuild artifact directory before retrying",
    }
}

fn budget_exhausted() -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_BUDGET_EXHAUSTED,
        message: "rebuild background budget exhausted".to_string(),
        remediation: "retry the rebuild when the PH43 background budget replenishes",
    }
}
