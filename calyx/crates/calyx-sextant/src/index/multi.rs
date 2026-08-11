//! Multi-vector token index with MaxSim late interaction.

use calyx_core::{CxId, Result, SlotId, SlotShape, SlotVector};

use super::{IndexSearchHit, IndexStats, SextantIndex, distance::dot, ranked};
use crate::util::top_k;

#[derive(Clone, Debug)]
pub struct MaxSimIndex {
    slot: SlotId,
    token_dim: u32,
    rows: Vec<(CxId, Vec<Vec<f32>>, u64)>,
    built_at_seq: u64,
    base_seq: u64,
}

impl MaxSimIndex {
    pub fn new(slot: SlotId, token_dim: u32) -> Self {
        Self {
            slot,
            token_dim,
            rows: Vec::new(),
            built_at_seq: 0,
            base_seq: 0,
        }
    }

    pub fn maxsim(query: &[Vec<f32>], doc: &[Vec<f32>]) -> f32 {
        // This and the persisted pinned scorer intentionally share Sextant's
        // runtime-dispatched dot reduction. Norms are computed once per token,
        // rather than once per query/document-token pair. AVX2 and scalar hosts
        // may differ in their last bits, but the live and persisted paths on a
        // given host remain bit-identical because both use this same kernel.
        let query_norms = query
            .iter()
            .map(|token| dot(token, token).sqrt())
            .collect::<Vec<_>>();
        let doc_norms = doc
            .iter()
            .map(|token| dot(token, token).sqrt())
            .collect::<Vec<_>>();
        query
            .iter()
            .zip(query_norms)
            .map(|(q, q_norm)| {
                doc.iter()
                    .zip(&doc_norms)
                    .map(|(d, d_norm)| {
                        if q_norm == 0.0 || *d_norm == 0.0 {
                            0.0
                        } else {
                            dot(q, d) / (q_norm * d_norm)
                        }
                    })
                    .fold(f32::NEG_INFINITY, f32::max)
            })
            .sum()
    }

    pub fn cpu_gpu_delta(_query: &[Vec<f32>], _doc: &[Vec<f32>]) -> Result<f32> {
        Err(crate::error::sextant_error(
            crate::error::CALYX_SEXTANT_GPU_PARITY_UNAVAILABLE,
            "MaxSimIndex has no wired Forge GPU MaxSim path; CPU/GPU delta is unavailable",
        ))
    }
}

impl SextantIndex for MaxSimIndex {
    fn slot(&self) -> SlotId {
        self.slot
    }

    fn shape(&self) -> SlotShape {
        SlotShape::Multi {
            token_dim: self.token_dim,
        }
    }

    fn insert(&mut self, cx_id: CxId, vector: SlotVector, seq: u64) -> Result<()> {
        let SlotVector::Multi { token_dim, tokens } = vector else {
            return Err(crate::error::sextant_error(
                crate::error::CALYX_SEXTANT_VECTOR_SHAPE,
                "multi index received non-multi vector",
            ));
        };
        if token_dim != self.token_dim {
            return Err(crate::error::sextant_error(
                crate::error::CALYX_SEXTANT_VECTOR_SHAPE,
                format!("token dim {token_dim} expected {}", self.token_dim),
            ));
        }
        if let Some(row) = self.rows.iter_mut().find(|row| row.0 == cx_id) {
            *row = (cx_id, tokens, seq);
        } else {
            self.rows.push((cx_id, tokens, seq));
        }
        self.built_at_seq = self.built_at_seq.max(seq);
        self.base_seq = self.base_seq.max(seq);
        Ok(())
    }

    fn search(
        &self,
        query: &SlotVector,
        k: usize,
        _ef: Option<usize>,
    ) -> Result<Vec<IndexSearchHit>> {
        let SlotVector::Multi { token_dim, tokens } = query else {
            return Err(crate::error::sextant_error(
                crate::error::CALYX_SEXTANT_VECTOR_SHAPE,
                "multi query required",
            ));
        };
        if *token_dim != self.token_dim {
            return Err(crate::error::sextant_error(
                crate::error::CALYX_SEXTANT_VECTOR_SHAPE,
                "token dim mismatch",
            ));
        }
        Ok(ranked(top_k(
            self.rows
                .iter()
                .map(|(cx, doc, _)| (*cx, Self::maxsim(tokens, doc)))
                .collect(),
            k,
        )))
    }

    fn rebuild(&mut self) -> Result<()> {
        self.built_at_seq = self.base_seq;
        Ok(())
    }

    fn vector(&self, cx_id: CxId) -> Option<SlotVector> {
        self.rows
            .iter()
            .find(|row| row.0 == cx_id)
            .map(|row| SlotVector::Multi {
                token_dim: self.token_dim,
                tokens: row.1.clone(),
            })
    }

    fn set_base_seq(&mut self, seq: u64) {
        self.base_seq = seq;
    }

    fn stats(&self) -> IndexStats {
        IndexStats {
            slot: self.slot,
            shape: self.shape(),
            len: self.rows.len(),
            built_at_seq: self.built_at_seq,
            base_seq: self.base_seq,
            kind: "multi_maxsim",
        }
    }
}
