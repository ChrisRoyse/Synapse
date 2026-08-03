//! Partitioned-vault manifest types and closure-assignment telemetry (#1129).

use std::path::Path;

use bincode::config;
use calyx_aster::cf::{CfRouter, ColumnFamily};
use calyx_core::Result;
use serde::{Deserialize, Serialize};

use super::{DiskAnnBuildBackend, PartitionDistanceMetric};

const MANIFEST_DB_KEY: &[u8] = b"calyx/partitioned-vault/manifest/v1/default";
const MANIFEST_DB_VALUE_MAGIC: &[u8] = b"CPARTM1\0";
const CF_MEMTABLE_CAP: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionMeta {
    pub id: u32,
    pub count: usize,
    pub graph_rel: String,
    pub ids_rel: String,
}

/// Build-time closure-assignment telemetry (#1129). SPTAG logs the equivalent
/// "RNG failed count" and a replica-count histogram at build time; without
/// these counters a `max_replication` request that the RNG rule prunes to
/// nothing is indistinguishable from working boundary replication.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClosureAssignmentStats {
    /// Rows routed through bounded closure assignment (equals n_cx).
    pub rows: u64,
    /// Replica copies stored beyond each row's primary region.
    pub replicas_stored: u64,
    /// Replica candidates rejected by the (1 + epsilon) closure threshold.
    pub epsilon_filtered: u64,
    /// Replica candidates rejected by the RNG rule.
    pub rng_skipped: u64,
    /// Replica candidates rejected by the region cap or per-region duplicate cap.
    pub cap_skipped: u64,
    /// Rows whose replication stopped early on the global duplicate budget.
    pub budget_stopped_rows: u64,
    /// `replica_histogram[i]` = rows stored in exactly `i + 1` regions.
    pub replica_histogram: Vec<u64>,
}

impl ClosureAssignmentStats {
    /// Stored copies per row (1.0 = no replication happened).
    pub fn replication_factor(&self) -> f64 {
        if self.rows == 0 {
            return 1.0;
        }
        1.0 + self.replicas_stored as f64 / self.rows as f64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionedManifest {
    pub format: String,
    pub n_cx: u64,
    pub dim: usize,
    pub n_regions: usize,
    pub seed: u64,
    pub m_max: usize,
    pub ef_construction: usize,
    #[serde(default)]
    pub distance_metric: PartitionDistanceMetric,
    #[serde(default)]
    pub region_build_parallelism: usize,
    #[serde(default = "default_graph_build_backend")]
    pub graph_build_backend: DiskAnnBuildBackend,
    #[serde(default)]
    pub provisional_assignment_routing: String,
    #[serde(default)]
    pub final_assignment_routing: String,
    #[serde(default)]
    pub final_assignment_probe: usize,
    #[serde(default)]
    pub final_assignment_cap: Option<usize>,
    #[serde(default)]
    pub final_assignment_boundary_epsilon: f32,
    #[serde(default)]
    pub final_assignment_max_replication: usize,
    #[serde(default)]
    pub final_assignment_rng_rule: bool,
    /// SPTAG `RNGFactor` parity: relaxes the RNG rule on the squared-distance
    /// scale. Manifests written before #1129 default to the strict paper rule.
    #[serde(default = "default_rng_factor")]
    pub final_assignment_rng_factor: f32,
    /// Closure telemetry; `None` for vaults built before #1129.
    #[serde(default)]
    pub final_assignment_closure: Option<ClosureAssignmentStats>,
    #[serde(default)]
    pub region_balance_cap: usize,
    #[serde(default)]
    pub stored_region_members: usize,
    pub centroids_rel: String,
    pub root_graph_rel: String,
    pub regions: Vec<RegionMeta>,
}

#[derive(Debug, Clone)]
pub struct PartitionedManifestDbReadback {
    pub manifest: PartitionedManifest,
    pub value_bytes: usize,
    pub value_blake3: String,
}

fn default_graph_build_backend() -> DiskAnnBuildBackend {
    DiskAnnBuildBackend::CpuVamana
}

pub(super) fn default_rng_factor() -> f32 {
    1.0
}

pub(super) fn write_manifest_db(root: &Path, manifest: &PartitionedManifest) -> Result<()> {
    let value = encode_manifest(manifest)?;
    let router = CfRouter::open(root, CF_MEMTABLE_CAP)?;
    router.put(ColumnFamily::Graph, MANIFEST_DB_KEY, &value)?;
    router.flush_cf(ColumnFamily::Graph)?;
    drop(router);

    let readback = read_manifest_bytes(root)?.ok_or_else(|| {
        crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_MISSING,
            "partitioned manifest Graph CF row missing after write",
        )
    })?;
    if readback != value {
        return Err(crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_MISMATCH,
            "partitioned manifest Graph CF readback bytes changed after write",
        ));
    }
    Ok(())
}

pub(super) fn read_manifest_db(root: &Path) -> Result<PartitionedManifest> {
    let value = read_manifest_bytes(root)?.ok_or_else(|| {
        crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_MISSING,
            "partitioned manifest Graph CF row is missing",
        )
    })?;
    decode_manifest(&value)
}

pub(super) fn manifest_db_exists(root: &Path) -> Result<bool> {
    if !graph_cf_dir(root).is_dir() {
        return Ok(false);
    }
    Ok(read_manifest_bytes(root)?.is_some())
}

pub(super) fn read_manifest_db_readback(root: &Path) -> Result<PartitionedManifestDbReadback> {
    let value = read_manifest_bytes(root)?.ok_or_else(|| {
        crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_MISSING,
            "partitioned manifest Graph CF row is missing",
        )
    })?;
    let manifest = decode_manifest(&value)?;
    Ok(PartitionedManifestDbReadback {
        manifest,
        value_bytes: value.len(),
        value_blake3: blake3::hash(&value).to_hex().to_string(),
    })
}

fn read_manifest_bytes(root: &Path) -> Result<Option<Vec<u8>>> {
    if !graph_cf_dir(root).is_dir() {
        return Ok(None);
    }
    let router = CfRouter::open(root, CF_MEMTABLE_CAP)?;
    router.get(ColumnFamily::Graph, MANIFEST_DB_KEY)
}

fn graph_cf_dir(root: &Path) -> std::path::PathBuf {
    root.join("cf").join(ColumnFamily::Graph.name())
}

fn encode_manifest(manifest: &PartitionedManifest) -> Result<Vec<u8>> {
    let payload = bincode::serde::encode_to_vec(manifest, config::standard()).map_err(|err| {
        crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_INVALID,
            format!("encode partitioned manifest DB row failed: {err}"),
        )
    })?;
    let mut value = Vec::with_capacity(MANIFEST_DB_VALUE_MAGIC.len() + payload.len());
    value.extend_from_slice(MANIFEST_DB_VALUE_MAGIC);
    value.extend_from_slice(&payload);
    Ok(value)
}

fn decode_manifest(value: &[u8]) -> Result<PartitionedManifest> {
    let payload = value.strip_prefix(MANIFEST_DB_VALUE_MAGIC).ok_or_else(|| {
        crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_INVALID,
            "partitioned manifest DB row has invalid magic",
        )
    })?;
    let (manifest, consumed): (PartitionedManifest, usize) =
        bincode::serde::decode_from_slice(payload, config::standard()).map_err(|err| {
            crate::error::sextant_error(
                crate::error::CALYX_INDEX_MANIFEST_DB_INVALID,
                format!("decode partitioned manifest DB row failed: {err}"),
            )
        })?;
    if consumed != payload.len() {
        return Err(crate::error::sextant_error(
            crate::error::CALYX_INDEX_MANIFEST_DB_INVALID,
            "partitioned manifest DB row has trailing bytes",
        ));
    }
    Ok(manifest)
}
