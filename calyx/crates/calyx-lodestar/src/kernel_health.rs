//! Engine-level `kernel_health(kernel_id)` aggregate (PRD 08 §8).
//!
//! Health is assembled by READING the persisted kernel artifact — never by
//! recomputing recall/groundedness. A missing or stale artifact fails closed
//! with a structured `CALYX_*` error; nothing is fabricated.

use std::fs;

use calyx_core::CxId;
use serde::{Deserialize, Serialize};

use crate::grounding_gaps::{CALYX_KERNEL_EMPTY, CALYX_KERNEL_UNGROUNDED};
use crate::{FsKernelStore, Kernel, LodestarError, RecallEvalParams, Result};

pub const KERNEL_ARTIFACT_FORMAT_VERSION: u32 = 2;

/// Persistence boundary for the full Kernel artifact (`kernel.json`),
/// the sibling of the kernel index (`index.json`).
pub trait KernelArtifactStore {
    fn write_kernel_bytes(&self, kernel_id: CxId, bytes: &[u8]) -> Result<()>;
    fn read_kernel_bytes(&self, kernel_id: CxId) -> Result<Option<Vec<u8>>>;
}

impl KernelArtifactStore for FsKernelStore {
    fn write_kernel_bytes(&self, kernel_id: CxId, bytes: &[u8]) -> Result<()> {
        let path = self.kernel_file_path(kernel_id);
        crate::kernel_index::install_immutable_file(&path, bytes)
    }

    fn read_kernel_bytes(&self, kernel_id: CxId) -> Result<Option<Vec<u8>>> {
        let path = self.kernel_file_path(kernel_id);
        if !path.exists() {
            return Ok(None);
        }
        fs::read(path).map(Some).map_err(io_error)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct KernelArtifactSnapshot {
    format_version: u32,
    kernel: Kernel,
}

pub fn write_kernel_artifact(kernel: &Kernel, store: &dyn KernelArtifactStore) -> Result<()> {
    let snapshot = KernelArtifactSnapshot {
        format_version: KERNEL_ARTIFACT_FORMAT_VERSION,
        kernel: kernel.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&snapshot).map_err(codec_error)?;
    store.write_kernel_bytes(kernel.kernel_id, &bytes)
}

pub fn read_kernel_artifact(kernel_id: CxId, store: &dyn KernelArtifactStore) -> Result<Kernel> {
    let Some(bytes) = store.read_kernel_bytes(kernel_id)? else {
        return Err(LodestarError::KernelNotFound { kernel_id });
    };
    let snapshot: KernelArtifactSnapshot = serde_json::from_slice(&bytes).map_err(codec_error)?;
    if snapshot.format_version != KERNEL_ARTIFACT_FORMAT_VERSION {
        return Err(LodestarError::KernelArtifactCodec {
            detail: format!(
                "unsupported kernel artifact format version {}",
                snapshot.format_version
            ),
        });
    }
    if snapshot.kernel.kernel_id != kernel_id {
        return Err(LodestarError::KernelArtifactCodec {
            detail: format!(
                "stale kernel artifact: stored kernel id {} did not match requested {}",
                snapshot.kernel.kernel_id, kernel_id
            ),
        });
    }
    Ok(snapshot.kernel)
}

/// A2 trust tag read from the persisted provenance — never assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelTrust {
    Anchored,
    Provisional,
    Empty,
}

/// Gate status of the persisted recall report (A10: measured, never assumed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallPassMode {
    Untested,
    Passed,
    BelowGate,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KernelRecallHealth {
    /// Measured kernel-only recall@k (raw).
    pub raw: f32,
    /// Acceptance metric: kernel-only / full ratio after tuning.
    pub tuned: f32,
    pub ratio: f32,
    pub min_recall_ratio: f32,
    pub n_queries_tested: usize,
    pub pass_mode: RecallPassMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KernelHealth {
    pub kernel_id: CxId,
    pub size: usize,
    pub kernel_graph_size: usize,
    pub recall: KernelRecallHealth,
    pub grounded_fraction: f32,
    pub unanchored_count: usize,
    pub approx_factor: f64,
    pub tau_star_estimate: usize,
    pub tau_star_exact: bool,
    pub built_at_millis: u64,
    pub panel_version: u32,
    pub anchor_kind: Option<String>,
    pub corpus_shard_hash: String,
    pub trust: KernelTrust,
    pub warnings: Vec<String>,
}

/// One-call kernel health: reads the persisted Kernel artifact and reports
/// size / recall / groundedness / approx-factor exactly as persisted.
pub fn kernel_health(kernel_id: CxId, store: &dyn KernelArtifactStore) -> Result<KernelHealth> {
    let kernel = read_kernel_artifact(kernel_id, store)?;
    Ok(kernel_health_from_kernel(&kernel))
}

/// Pure assembler from an already-loaded Kernel (no recomputation).
pub fn kernel_health_from_kernel(kernel: &Kernel) -> KernelHealth {
    KernelHealth {
        kernel_id: kernel.kernel_id,
        size: kernel.members.len(),
        kernel_graph_size: kernel.kernel_graph.len(),
        recall: recall_health(kernel),
        grounded_fraction: kernel.groundedness.reached_anchor,
        unanchored_count: kernel.groundedness.unanchored_members.len(),
        approx_factor: kernel.recall.approx_factor,
        tau_star_estimate: kernel.recall.tau_star_estimate,
        tau_star_exact: kernel.recall.tau_star_exact,
        built_at_millis: kernel.built_at_millis,
        panel_version: kernel.panel_version,
        anchor_kind: kernel.anchor_kind.clone(),
        corpus_shard_hash: hex(&kernel.corpus_shard_hash),
        trust: trust_tag(kernel),
        warnings: kernel.warnings.clone(),
    }
}

fn recall_health(kernel: &Kernel) -> KernelRecallHealth {
    let recall = &kernel.recall;
    let min_recall_ratio = recall
        .recall_eval_params
        .as_ref()
        .map(|params| params.min_recall_ratio)
        .unwrap_or_else(|| RecallEvalParams::default().min_recall_ratio);
    let pass_mode = if recall.n_queries_tested == 0 {
        RecallPassMode::Untested
    } else if recall.warning.is_none() && recall.ratio >= min_recall_ratio {
        RecallPassMode::Passed
    } else {
        RecallPassMode::BelowGate
    };
    KernelRecallHealth {
        raw: recall.kernel_only,
        tuned: recall.ratio,
        ratio: recall.ratio,
        min_recall_ratio,
        n_queries_tested: recall.n_queries_tested,
        pass_mode,
    }
}

fn trust_tag(kernel: &Kernel) -> KernelTrust {
    if kernel.members.is_empty()
        || kernel.estimator_provenance.contains("trust=empty")
        || kernel
            .warnings
            .iter()
            .any(|warning| warning.starts_with(CALYX_KERNEL_EMPTY))
    {
        return KernelTrust::Empty;
    }
    let ungrounded_warning = kernel
        .warnings
        .iter()
        .any(|warning| warning.starts_with(CALYX_KERNEL_UNGROUNDED));
    if ungrounded_warning || kernel.estimator_provenance.contains("trust=provisional") {
        KernelTrust::Provisional
    } else {
        KernelTrust::Anchored
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn io_error(err: std::io::Error) -> LodestarError {
    LodestarError::KernelIndexIo {
        detail: err.to_string(),
    }
}

fn codec_error(err: serde_json::Error) -> LodestarError {
    LodestarError::KernelArtifactCodec {
        detail: err.to_string(),
    }
}
