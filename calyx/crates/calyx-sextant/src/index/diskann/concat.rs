//! DiskANN over materialized concat cross-term (`xterm`) vectors.

use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use calyx_core::{CxId, PanelSlotId, Result, SlotId};

use super::{DiskAnnBuildParams, DiskAnnSearch, DiskAnnSearchParams};
use crate::error::{
    CALYX_INDEX_DIM_MISMATCH, CALYX_INDEX_INVALID_PARAMS, CALYX_INDEX_IO, sextant_error,
};

const KEYS_MAGIC: [u8; 8] = *b"CLXXTRM2";
const KEYS_FORMAT_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConcatCrossTermKey {
    pub cx_id: CxId,
    pub a: PanelSlotId,
    pub b: PanelSlotId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConcatCrossTermHit {
    pub key: ConcatCrossTermKey,
    pub distance: f32,
}

#[derive(Debug)]
pub struct ConcatCrossTermDiskAnn {
    panel_version: u32,
    dim: u32,
    root: PathBuf,
    graph: DiskAnnSearch,
    keys: Vec<ConcatCrossTermKey>,
    default_search: DiskAnnSearchParams,
}

impl ConcatCrossTermDiskAnn {
    pub fn build(
        root: impl Into<PathBuf>,
        rows: &[(ConcatCrossTermKey, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        default_search: DiskAnnSearchParams,
    ) -> Result<Self> {
        let panel_version = validate_rows(rows, build_params.dim)?;
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io("create concat index dir", e))?;
        let graph_rows: Vec<_> = rows
            .iter()
            .map(|(key, vector)| (key.cx_id, vector.clone()))
            .collect();
        let graph = DiskAnnSearch::build(
            SlotId::new(0),
            graph_path(&root),
            &graph_rows,
            build_params,
            None,
            default_search,
        )?;
        let keys: Vec<_> = rows.iter().map(|(key, _)| *key).collect();
        write_keys(&keys_path(&root), build_params.dim as u32, &keys)?;
        Ok(Self {
            panel_version,
            dim: build_params.dim as u32,
            root,
            graph,
            keys,
            default_search,
        })
    }

    pub fn open(root: impl Into<PathBuf>, default_search: DiskAnnSearchParams) -> Result<Self> {
        let root = root.into();
        let (dim, keys) = read_keys(&keys_path(&root))?;
        let panel_version = validate_key_panels(&keys)?;
        let graph = DiskAnnSearch::open(
            SlotId::new(0),
            graph_path(&root),
            keys.iter().map(|key| key.cx_id).collect(),
            None,
            default_search,
        )?;
        Ok(Self {
            panel_version,
            dim,
            root,
            graph,
            keys,
            default_search,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub const fn panel_version(&self) -> u32 {
        self.panel_version
    }

    pub fn graph_path(&self) -> PathBuf {
        graph_path(&self.root)
    }

    pub fn search_terms(
        &self,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<ConcatCrossTermHit>> {
        if query.len() != self.dim as usize {
            return Err(sextant_error(
                CALYX_INDEX_DIM_MISMATCH,
                format!("concat query dim {} expected {}", query.len(), self.dim),
            ));
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(invalid("concat query has non-finite component"));
        }
        let mut params = self.default_search;
        if let Some(ef) = ef {
            params.ef_search = ef;
        }
        self.graph
            .search_ids(query, k, &params)?
            .into_iter()
            .map(|(id, distance)| {
                let key = self
                    .keys
                    .get(id as usize)
                    .copied()
                    .ok_or_else(|| invalid(format!("concat node {id} has no key")))?;
                Ok(ConcatCrossTermHit { key, distance })
            })
            .collect()
    }
}

fn validate_rows(rows: &[(ConcatCrossTermKey, Vec<f32>)], dim: usize) -> Result<u32> {
    if rows.is_empty() {
        return Err(invalid(
            "empty input: at least one concat cross-term is required",
        ));
    }
    let panel_version = validate_key_panels(&rows.iter().map(|(key, _)| *key).collect::<Vec<_>>())?;
    for (idx, (_, vector)) in rows.iter().enumerate() {
        if vector.len() != dim {
            return Err(sextant_error(
                CALYX_INDEX_DIM_MISMATCH,
                format!("concat row {idx} dim {} expected {dim}", vector.len()),
            ));
        }
        if vector.iter().any(|v| !v.is_finite()) {
            return Err(invalid(format!(
                "concat row {idx} has non-finite component"
            )));
        }
    }
    Ok(panel_version)
}

fn validate_key_panels(keys: &[ConcatCrossTermKey]) -> Result<u32> {
    let first = keys
        .first()
        .ok_or_else(|| invalid("empty concat cross-term key set"))?;
    let panel_version = first.a.panel_version();
    for key in keys {
        if key.a.panel_version() != panel_version || key.b.panel_version() != panel_version {
            return Err(invalid(format!(
                "concat xterm index mixes {} and {} with required panel {panel_version}",
                key.a, key.b
            )));
        }
    }
    Ok(panel_version)
}

fn write_keys(path: &Path, dim: u32, keys: &[ConcatCrossTermKey]) -> Result<()> {
    let mut out = BufWriter::new(File::create(path).map_err(|e| io("create keys sidecar", e))?);
    out.write_all(&KEYS_MAGIC)
        .map_err(|e| io("write keys magic", e))?;
    out.write_all(&KEYS_FORMAT_VERSION.to_le_bytes())
        .map_err(|e| io("write keys version", e))?;
    out.write_all(&dim.to_le_bytes())
        .map_err(|e| io("write keys dim", e))?;
    out.write_all(&(keys.len() as u64).to_le_bytes())
        .map_err(|e| io("write keys count", e))?;
    for key in keys {
        out.write_all(key.cx_id.as_bytes())
            .map_err(|e| io("write key cx", e))?;
        out.write_all(&key.a.panel_version().to_le_bytes())
            .map_err(|e| io("write key panel a", e))?;
        out.write_all(&key.a.slot_id().get().to_le_bytes())
            .map_err(|e| io("write key slot a", e))?;
        out.write_all(&key.b.panel_version().to_le_bytes())
            .map_err(|e| io("write key panel b", e))?;
        out.write_all(&key.b.slot_id().get().to_le_bytes())
            .map_err(|e| io("write key slot b", e))?;
    }
    out.into_inner()
        .map_err(|e| io("flush keys", e.into_error()))?
        .sync_all()
        .map_err(|e| io("fsync keys", e))
}

fn read_keys(path: &Path) -> Result<(u32, Vec<ConcatCrossTermKey>)> {
    let bytes = fs::read(path).map_err(|e| io("read keys sidecar", e))?;
    if bytes.len() < 24 || bytes[0..8] != KEYS_MAGIC {
        return Err(invalid("bad concat key sidecar header"));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("4B"));
    if version != KEYS_FORMAT_VERSION {
        return Err(invalid(format!("concat key sidecar version {version}")));
    }
    let dim = u32::from_le_bytes(bytes[12..16].try_into().expect("4B"));
    let count = u64::from_le_bytes(bytes[16..24].try_into().expect("8B")) as usize;
    let expected = 24 + count * 28;
    if bytes.len() != expected {
        return Err(invalid(format!(
            "concat key sidecar len {} != {expected}",
            bytes.len()
        )));
    }
    let mut keys = Vec::with_capacity(count);
    for chunk in bytes[24..].chunks_exact(28) {
        let mut id = [0_u8; 16];
        id.copy_from_slice(&chunk[..16]);
        keys.push(ConcatCrossTermKey {
            cx_id: CxId::from_bytes(id),
            a: PanelSlotId::new(
                u32::from_le_bytes(chunk[16..20].try_into().expect("4B")),
                SlotId::new(u16::from_le_bytes(chunk[20..22].try_into().expect("2B"))),
            ),
            b: PanelSlotId::new(
                u32::from_le_bytes(chunk[22..26].try_into().expect("4B")),
                SlotId::new(u16::from_le_bytes(chunk[26..28].try_into().expect("2B"))),
            ),
        });
    }
    Ok((dim, keys))
}

fn graph_path(root: &Path) -> PathBuf {
    root.join("graph.cda")
}

fn keys_path(root: &Path) -> PathBuf {
    root.join("keys.cdx")
}

fn invalid(detail: impl std::fmt::Display) -> calyx_core::CalyxError {
    sextant_error(
        CALYX_INDEX_INVALID_PARAMS,
        format!("concat diskann invalid params: {detail}"),
    )
}

fn io(stage: &str, error: std::io::Error) -> calyx_core::CalyxError {
    sextant_error(CALYX_INDEX_IO, format!("concat diskann {stage}: {error}"))
}
