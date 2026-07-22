use std::path::Path;

use calyx_core::Result;

use super::helpers::{distance_to_node, sorted};
use super::{DiskAnnSearch, DiskAnnSearchParams, SearchBuildSidecars};
use crate::index::diskann::build::{DiskAnnBuildBackend, DiskAnnBuildParams};
use crate::index::diskann::pq::{
    DiskAnnPqBuildExecution, DiskAnnPqBuildParams, DiskAnnPqIndex, default_pq_sidecar,
};
use crate::index::distance::l2_normalize;
use calyx_core::{CxId, SlotId};

#[derive(Clone, Copy, Debug)]
pub struct DiskAnnPqSearchBuild {
    pub search: DiskAnnSearchParams,
    pub pq: DiskAnnPqBuildParams,
    pub backend: DiskAnnBuildBackend,
}

impl DiskAnnSearch {
    pub fn build_with_pq(
        slot: SlotId,
        graph_path: impl Into<std::path::PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<std::path::PathBuf>,
        default_search: DiskAnnSearchParams,
        pq_params: DiskAnnPqBuildParams,
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
                pq: Some(pq_params),
                backend: DiskAnnBuildBackend::CpuVamana,
            },
        )
    }

    pub fn build_with_pq_plan(
        slot: SlotId,
        graph_path: impl Into<std::path::PathBuf>,
        rows: &[(CxId, Vec<f32>)],
        build_params: DiskAnnBuildParams,
        raw_sidecar: Option<std::path::PathBuf>,
        plan: DiskAnnPqSearchBuild,
    ) -> Result<Self> {
        Self::build_with_default_raw_sidecar(
            slot,
            graph_path,
            rows,
            build_params,
            raw_sidecar,
            plan.search,
            SearchBuildSidecars {
                write_default_raw_sidecar: true,
                pq: Some(plan.pq),
                backend: plan.backend,
            },
        )
    }

    pub(super) fn rescore_final(
        &self,
        query: &[f32],
        graph_query: &[f32],
        hits: &[(u32, f32)],
        pq_scored: bool,
    ) -> Result<Vec<(u32, f32)>> {
        // Classify the sidecar once (opening/validating the packed file if
        // present); a corrupt packed file or a symlink fails closed here.
        let source = self.classify_raw()?;
        if source.has_vectors() {
            return self.rescore_from_raw(&source, query, hits);
        }
        if pq_scored {
            return self.rescore_from_graph(graph_query, hits);
        }
        Ok(hits.to_vec())
    }

    pub fn pq_ram_bytes(&self) -> Option<usize> {
        self.pq.as_ref().map(DiskAnnPqIndex::ram_bytes)
    }

    pub fn pq_summary(&self) -> Option<(usize, usize, usize)> {
        self.pq
            .as_ref()
            .map(|pq| (pq.node_count(), pq.subvectors(), pq.centroids()))
    }

    pub fn pq_build_diagnostics(
        &self,
    ) -> Option<&crate::index::diskann::pq::DiskAnnPqBuildDiagnostics> {
        self.pq.as_ref().map(DiskAnnPqIndex::build_diagnostics)
    }

    fn rescore_from_graph(
        &self,
        graph_query: &[f32],
        hits: &[(u32, f32)],
    ) -> Result<Vec<(u32, f32)>> {
        let Some(reader) = &self.reader else {
            return Ok(hits.to_vec());
        };
        let rescored: Result<Vec<_>> = hits
            .iter()
            .map(|&(id, _)| {
                let node = reader.read_node(id)?;
                Ok((
                    id,
                    distance_to_node(graph_query, node.vector, self.distance_mode),
                ))
            })
            .collect();
        Ok(sorted(rescored?))
    }
}

pub(super) fn write_pq_sidecar(
    graph_path: &Path,
    dense_rows: &[(u32, Vec<f32>)],
    pq_params: DiskAnnPqBuildParams,
    graph_backend: DiskAnnBuildBackend,
) -> Result<DiskAnnPqIndex> {
    let graph_rows: Vec<_> = dense_rows
        .iter()
        .map(|(id, vector)| (*id, l2_normalize(vector)))
        .collect();
    let execution = match graph_backend {
        DiskAnnBuildBackend::CuvsCagra => DiskAnnPqBuildExecution::CudaRequired,
        DiskAnnBuildBackend::CpuVamana => DiskAnnPqBuildExecution::Auto,
    };
    let pq = DiskAnnPqIndex::build_with_execution(&graph_rows, pq_params, execution)?;
    pq.write_atomic(&default_pq_sidecar(graph_path))?;
    Ok(pq)
}
