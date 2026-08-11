mod launch;
mod mxfp;
mod mxfp_launch;
mod packed;
mod packed_launch;
mod scores;
mod turboquant;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::CudaContext;

pub use mxfp::CudaMxFpBatch;
pub use packed::{CudaBinaryBatch, CudaBinaryScores, CudaInt8Batch};
pub use scores::CudaQuantScores;
pub use turboquant::CudaTurboQuantBatch;
pub type CudaTurboQuantScores = CudaQuantScores;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CudaQuantStats {
    pub kernel_launches: u64,
    pub h2d_bytes: u64,
    pub d2h_bytes: u64,
    pub encoded_rows: u64,
    pub scored_candidates: u64,
    pub compact_topk_rows: u64,
}

#[derive(Default)]
pub(super) struct QuantCounters {
    kernel_launches: AtomicU64,
    h2d_bytes: AtomicU64,
    d2h_bytes: AtomicU64,
    encoded_rows: AtomicU64,
    scored_candidates: AtomicU64,
    compact_topk_rows: AtomicU64,
}

impl QuantCounters {
    pub(super) fn add_launches(&self, value: u64) {
        self.kernel_launches.fetch_add(value, Ordering::Relaxed);
    }

    pub(super) fn add_h2d(&self, value: usize) {
        self.h2d_bytes.fetch_add(value as u64, Ordering::Relaxed);
    }

    pub(super) fn add_d2h(&self, value: usize) {
        self.d2h_bytes.fetch_add(value as u64, Ordering::Relaxed);
    }

    pub(super) fn add_encoded_rows(&self, value: usize) {
        self.encoded_rows.fetch_add(value as u64, Ordering::Relaxed);
    }

    pub(super) fn add_scored_candidates(&self, value: usize) {
        self.scored_candidates
            .fetch_add(value as u64, Ordering::Relaxed);
    }

    pub(super) fn add_compact_topk_rows(&self, value: usize) {
        self.compact_topk_rows
            .fetch_add(value as u64, Ordering::Relaxed);
    }

    fn snapshot(&self) -> CudaQuantStats {
        CudaQuantStats {
            kernel_launches: self.kernel_launches.load(Ordering::Relaxed),
            h2d_bytes: self.h2d_bytes.load(Ordering::Relaxed),
            d2h_bytes: self.d2h_bytes.load(Ordering::Relaxed),
            encoded_rows: self.encoded_rows.load(Ordering::Relaxed),
            scored_candidates: self.scored_candidates.load(Ordering::Relaxed),
            compact_topk_rows: self.compact_topk_rows.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
pub struct CudaQuantContext {
    ctx: CudaContext,
    counters: Arc<QuantCounters>,
}

impl CudaQuantContext {
    pub fn new(ctx: CudaContext) -> Self {
        Self {
            ctx,
            counters: Arc::new(QuantCounters::default()),
        }
    }

    pub fn context(&self) -> &CudaContext {
        &self.ctx
    }

    pub fn stats(&self) -> CudaQuantStats {
        self.counters.snapshot()
    }

    pub fn reset_stats(&self) {
        self.counters.kernel_launches.store(0, Ordering::Relaxed);
        self.counters.h2d_bytes.store(0, Ordering::Relaxed);
        self.counters.d2h_bytes.store(0, Ordering::Relaxed);
        self.counters.encoded_rows.store(0, Ordering::Relaxed);
        self.counters.scored_candidates.store(0, Ordering::Relaxed);
        self.counters.compact_topk_rows.store(0, Ordering::Relaxed);
    }

    pub(super) fn counters(&self) -> Arc<QuantCounters> {
        self.counters.clone()
    }
}

impl std::fmt::Debug for CudaQuantContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CudaQuantContext")
            .field("device_idx", &self.ctx.device_idx())
            .field("stats", &self.stats())
            .finish()
    }
}
