use std::path::{Path, PathBuf};

use calyx_core::{CxId, Result, SlotId};

use super::helpers::{dense_rows, invalid, open_for_search, positions};
use super::pq_support::write_pq_sidecar;
use super::storage::{
    build_search_graph_raw_l2_with_backend, build_search_graph_with_backend_and_progress,
    default_raw_sidecar, read_distance_mode,
};
use super::{DiskAnnSearch, DiskAnnSearchParams, SearchBuildSidecars, prefetch_file_for_graph};
use crate::index::diskann::build::{DiskAnnBuildBackend, DiskAnnBuildParams, DiskAnnBuildProgress};
use crate::index::diskann::pq::{DiskAnnPqIndex, default_pq_sidecar};

impl DiskAnnSearch {
    pub fn open(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        ids: Vec<CxId>,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
    ) -> Result<Self> {
        let graph_path = graph_path.into();
        let reader = open_for_search(&graph_path)?;
        let header = *reader.header();
        let distance_mode = read_distance_mode(&graph_path)?;
        if ids.len() != header.node_count as usize {
            return Err(invalid(format!(
                "id map len {} != graph node_count {}",
                ids.len(),
                header.node_count
            )));
        }
        let raw_sidecar = raw_sidecar.or_else(|| {
            // Accept both the packed v2 file and a legacy v1 directory; the read
            // path classifies and fails closed on anything else (#1990).
            let path = default_raw_sidecar(&graph_path);
            (path.is_file() || path.is_dir()).then_some(path)
        });
        let pq = DiskAnnPqIndex::read_if_exists(&default_pq_sidecar(&graph_path))?;
        let graph_file = prefetch_file_for_graph(&graph_path, &reader)?;
        let build_params = DiskAnnBuildParams {
            dim: header.dim as usize,
            m_max: header.m_max as usize,
            ef_construction: default_search.ef_search.max(header.m_max as usize),
            alpha: 1.2,
        };
        Ok(Self {
            slot,
            dim: header.dim,
            graph_path,
            raw_sidecar,
            pq,
            reader: Some(reader),
            graph_file,
            distance_mode,
            positions: positions(&ids),
            ids,
            build_params,
            build_backend: DiskAnnBuildBackend::CpuVamana,
            default_search,
            built_at_seq: 0,
            base_seq: 0,
        })
    }

    pub fn build(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
    ) -> Result<Self> {
        Self::build_with_default_raw_sidecar(
            slot,
            graph_path,
            rows,
            build_params,
            raw_sidecar,
            default_search,
            SearchBuildSidecars {
                write_default_raw_sidecar: true,
                pq: None,
                backend: DiskAnnBuildBackend::CpuVamana,
            },
        )
    }

    pub fn build_with_backend(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
        backend: DiskAnnBuildBackend,
    ) -> Result<Self> {
        Self::build_with_default_raw_sidecar(
            slot,
            graph_path,
            rows,
            build_params,
            raw_sidecar,
            default_search,
            SearchBuildSidecars {
                write_default_raw_sidecar: true,
                pq: None,
                backend,
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_with_backend_and_progress<F>(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
        backend: DiskAnnBuildBackend,
        progress: F,
    ) -> Result<Self>
    where
        F: FnMut(DiskAnnBuildProgress) -> Result<()>,
    {
        Self::build_with_default_raw_sidecar_and_progress(
            slot,
            graph_path,
            rows,
            build_params,
            raw_sidecar,
            default_search,
            SearchBuildSidecars {
                write_default_raw_sidecar: true,
                pq: None,
                backend,
            },
            progress,
        )
    }

    pub(crate) fn build_without_default_raw_sidecar_with_backend(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
        backend: DiskAnnBuildBackend,
    ) -> Result<Self> {
        Self::build_with_default_raw_sidecar(
            slot,
            graph_path,
            rows,
            build_params,
            raw_sidecar,
            default_search,
            SearchBuildSidecars {
                write_default_raw_sidecar: false,
                pq: None,
                backend,
            },
        )
    }

    pub(crate) fn build_raw_l2_without_default_raw_sidecar_with_backend(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
        backend: DiskAnnBuildBackend,
    ) -> Result<Self> {
        let graph_path = graph_path.into();
        let dense_rows = dense_rows(rows, build_params.dim)?;
        build_search_graph_raw_l2_with_backend(
            &graph_path,
            &dense_rows,
            build_params,
            raw_sidecar,
            false,
            backend,
        )?;
        let mut search = Self::open(
            slot,
            graph_path,
            rows.iter().map(|(cx_id, _)| *cx_id).collect(),
            None,
            default_search,
        )?;
        search.build_backend = backend;
        Ok(search)
    }

    pub(super) fn build_with_default_raw_sidecar(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
        sidecars: SearchBuildSidecars,
    ) -> Result<Self> {
        Self::build_with_default_raw_sidecar_and_progress(
            slot,
            graph_path,
            rows,
            build_params,
            raw_sidecar,
            default_search,
            sidecars,
            |_| Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_with_default_raw_sidecar_and_progress<F>(
        slot: SlotId,
        graph_path: impl Into<PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<PathBuf>,
        default_search: DiskAnnSearchParams,
        sidecars: SearchBuildSidecars,
        progress: F,
    ) -> Result<Self>
    where
        F: FnMut(DiskAnnBuildProgress) -> Result<()>,
    {
        let graph_path = graph_path.into();
        let dense_rows = dense_rows(rows, build_params.dim)?;
        let write_raw_sidecar = raw_sidecar.is_none() && sidecars.write_default_raw_sidecar;
        let raw_sidecar = build_search_graph_with_backend_and_progress(
            &graph_path,
            &dense_rows,
            build_params,
            raw_sidecar,
            write_raw_sidecar,
            sidecars.backend,
            progress,
        )?;
        let built_pq = sidecars
            .pq
            .map(|pq_params| {
                write_pq_sidecar(&graph_path, &dense_rows, pq_params, sidecars.backend)
            })
            .transpose()?;
        let mut search = Self::open(
            slot,
            graph_path,
            rows.iter().map(|(cx_id, _)| *cx_id).collect(),
            raw_sidecar,
            default_search,
        )?;
        if let Some(pq) = built_pq {
            search.pq = Some(pq);
        }
        search.build_backend = sidecars.backend;
        Ok(search)
    }

    pub fn empty(slot: SlotId, dim: u32, graph_path: impl Into<PathBuf>) -> Self {
        Self {
            slot,
            dim,
            graph_path: graph_path.into(),
            raw_sidecar: None,
            pq: None,
            reader: None,
            graph_file: None,
            distance_mode: super::helpers::DiskAnnDistanceMode::UnitL2,
            ids: Vec::new(),
            positions: std::collections::HashMap::new(),
            build_params: DiskAnnBuildParams {
                dim: dim as usize,
                m_max: 32,
                ef_construction: 64,
                alpha: 1.2,
            },
            build_backend: DiskAnnBuildBackend::CpuVamana,
            default_search: DiskAnnSearchParams::default(),
            built_at_seq: 0,
            base_seq: 0,
        }
    }

    pub fn persist_path(&self) -> &Path {
        &self.graph_path
    }
}
