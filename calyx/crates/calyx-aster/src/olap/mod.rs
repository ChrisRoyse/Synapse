mod types;

pub use types::{
    DEFAULT_MAX_GROUPS, DEFAULT_MAX_ROWS, OlapAggregate, OlapGroupAggregate, OlapScanPlan,
    OlapScanResult,
};

use crate::mmap_col::MmapColumn;
use crate::mvcc::Snapshot;
use crate::sst::arrow::{ArrowColumnView, decode_column_shape};
use crate::vault::{AsterVault, SlotColumnManifest};
use calyx_core::{CalyxError, Clock, PanelSlotId, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST_MAGIC: &str = "CXSC2";
const MANIFEST_VERSION: u32 = 2;
const CHUNK_FILE: &str = "slot-column.cxa1";

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Materializes and aggregates through one caller-owned MVCC snapshot.
    pub fn olap_scan_aggregate_slot_at(
        &self,
        snapshot: Snapshot,
        panel_slot: PanelSlotId,
        output_dir: impl AsRef<Path>,
        plan: OlapScanPlan,
    ) -> Result<OlapScanResult> {
        let materialized = self.materialize_slot_column_at(snapshot, panel_slot, output_dir)?;
        scan_materialized_slot_column_aggregate(&materialized.manifest_path, plan)
    }
}

pub fn scan_materialized_slot_column_aggregate(
    manifest_path: impl AsRef<Path>,
    plan: OlapScanPlan,
) -> Result<OlapScanResult> {
    let manifest_path = manifest_path.as_ref();
    let manifest = read_manifest(manifest_path)?;
    let chunk_path = chunk_path_for(manifest_path, &manifest)?;
    let column = MmapColumn::open(&chunk_path)?;
    let chunk_sha256 = sha256_hex(column.as_bytes());
    if chunk_sha256 != manifest.chunk_sha256 {
        return Err(CalyxError::aster_corrupt_shard(
            "slot column chunk sha256 mismatch",
        ));
    }
    let chunk = decode_column_shape(column.as_bytes())?;
    validate_manifest_shape(&manifest, &chunk)?;
    validate_plan(plan, chunk.dim(), chunk.n_rows())?;
    let aggregate = scan_total(&chunk, plan.value_column)?;
    let groups = scan_groups(&chunk, plan)?;
    Ok(OlapScanResult {
        source_manifest_path: manifest_path.to_path_buf(),
        source_chunk_path: chunk_path,
        chunk_sha256,
        rows_scanned: chunk.n_rows(),
        dim: chunk.dim(),
        value_column: plan.value_column,
        group_by_column: plan.group_by_column,
        aggregate,
        groups,
    })
}

fn scan_total(chunk: &ArrowColumnView<'_>, column: usize) -> Result<OlapAggregate> {
    let mut acc = Accumulator::default();
    for value in chunk.column_values(column)? {
        acc.push(value)?;
    }
    acc.finish()
}

fn scan_groups(chunk: &ArrowColumnView<'_>, plan: OlapScanPlan) -> Result<Vec<OlapGroupAggregate>> {
    let Some(group_column) = plan.group_by_column else {
        return Ok(Vec::new());
    };
    let mut groups = BTreeMap::<u32, Accumulator>::new();
    for row in 0..chunk.n_rows() {
        let group_key = finite(chunk.value(group_column, row)?)?;
        let value = chunk.value(plan.value_column, row)?;
        if !groups.contains_key(&group_key.to_bits()) && groups.len() == plan.max_groups {
            return Err(olap_error(
                "CALYX_OLAP_SCAN_LIMIT",
                format!("group cap {} exceeded", plan.max_groups),
            ));
        }
        groups.entry(group_key.to_bits()).or_default().push(value)?;
    }
    groups
        .into_iter()
        .map(|(group_key_bits, acc)| {
            Ok(OlapGroupAggregate {
                group_key_bits,
                group_key: f32::from_bits(group_key_bits),
                aggregate: acc.finish()?,
            })
        })
        .collect()
}

fn validate_plan(plan: OlapScanPlan, dim: usize, rows: usize) -> Result<()> {
    if plan.max_rows == 0 {
        return Err(olap_error(
            "CALYX_OLAP_INVALID_PLAN",
            "max_rows must be > 0",
        ));
    }
    if rows > plan.max_rows {
        return Err(olap_error(
            "CALYX_OLAP_SCAN_LIMIT",
            format!("row cap {} exceeded by {rows}", plan.max_rows),
        ));
    }
    if plan.value_column >= dim {
        return Err(olap_error(
            "CALYX_OLAP_INVALID_PLAN",
            format!("value column {} outside dim {dim}", plan.value_column),
        ));
    }
    if let Some(group_by) = plan.group_by_column {
        if group_by >= dim {
            return Err(olap_error(
                "CALYX_OLAP_INVALID_PLAN",
                format!("group column {group_by} outside dim {dim}"),
            ));
        }
        if plan.max_groups == 0 {
            return Err(olap_error(
                "CALYX_OLAP_INVALID_PLAN",
                "max_groups must be > 0 when group_by is set",
            ));
        }
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<SlotColumnManifest> {
    let bytes = fs::read(path)
        .map_err(|error| olap_error("CALYX_OLAP_IO", format!("read manifest: {error}")))?;
    let manifest: SlotColumnManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CalyxError::aster_corrupt_shard(format!("decode slot column manifest: {error}"))
    })?;
    if manifest.magic != MANIFEST_MAGIC || manifest.version != MANIFEST_VERSION {
        return Err(CalyxError::aster_corrupt_shard(
            "slot column manifest version mismatch",
        ));
    }
    Ok(manifest)
}

fn chunk_path_for(manifest_path: &Path, manifest: &SlotColumnManifest) -> Result<PathBuf> {
    if manifest.chunk_file != CHUNK_FILE {
        return Err(CalyxError::aster_corrupt_shard(
            "slot column manifest chunk path invalid",
        ));
    }
    let parent = manifest_path
        .parent()
        .ok_or_else(|| olap_error("CALYX_OLAP_IO", "slot manifest has no parent"))?;
    Ok(parent.join(CHUNK_FILE))
}

fn validate_manifest_shape(
    manifest: &SlotColumnManifest,
    chunk: &ArrowColumnView<'_>,
) -> Result<()> {
    if chunk.n_rows() != manifest.rows || chunk.dim() != manifest.dim as usize {
        return Err(CalyxError::aster_corrupt_shard(
            "slot column manifest shape mismatch",
        ));
    }
    if manifest.cx_ids.len() != manifest.rows {
        return Err(CalyxError::aster_corrupt_shard(
            "slot column cx_id count mismatch",
        ));
    }
    Ok(())
}

#[derive(Debug, Default, Clone)]
struct Accumulator {
    count: usize,
    sum: f64,
    min: f32,
    max: f32,
}

impl Accumulator {
    fn push(&mut self, value: f32) -> Result<()> {
        let value = finite(value)?;
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        self.sum += f64::from(value);
        Ok(())
    }

    fn finish(self) -> Result<OlapAggregate> {
        if self.count == 0 {
            return Err(olap_error("CALYX_OLAP_EMPTY", "aggregate has no rows"));
        }
        Ok(OlapAggregate {
            count: self.count,
            sum: self.sum,
            min: self.min,
            max: self.max,
            avg: self.sum / self.count as f64,
        })
    }
}

fn finite(value: f32) -> Result<f32> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(olap_error(
            "CALYX_OLAP_NONFINITE_VALUE",
            "column aggregate encountered NaN or Inf",
        ))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn olap_error(code: &'static str, message: impl Into<String>) -> CalyxError {
    CalyxError {
        code,
        message: message.into(),
        remediation: "fix OLAP scan input or rebuild the materialized column chunk",
    }
}
