use std::collections::BTreeSet;

use calyx_core::{Clock, CxId, SlotVector, SystemClock};
use calyx_sextant::{HnswIndex, SextantIndex};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{KernelIndex, LodestarError, RecallReport, Result, kernel_search};

pub const CALYX_KERNEL_RECALL_BELOW_GATE: &str = "CALYX_KERNEL_RECALL_BELOW_GATE";

const DEFAULT_HELD_OUT_FRACTION: f32 = 0.1;
const DEFAULT_TOP_K: usize = 10;
const DEFAULT_RNG_SEED: u64 = 42;
const DEFAULT_MIN_RECALL_RATIO: f32 = 0.95;

pub trait AnnIndex {
    fn search(&self, query_vec: &[f32], top_k: usize) -> Result<Vec<(CxId, f32)>>;
}

pub trait CorpusReader {
    fn name(&self) -> &str;
    fn len(&self) -> usize;
    fn query(&self, ordinal: usize) -> Result<RecallQuery>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallEvalParams {
    pub held_out_fraction: f32,
    pub top_k: usize,
    pub rng_seed: u64,
    pub min_recall_ratio: f32,
}

impl Default for RecallEvalParams {
    fn default() -> Self {
        Self {
            held_out_fraction: DEFAULT_HELD_OUT_FRACTION,
            top_k: DEFAULT_TOP_K,
            rng_seed: DEFAULT_RNG_SEED,
            min_recall_ratio: DEFAULT_MIN_RECALL_RATIO,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallQuery {
    pub cx_id: CxId,
    pub vector: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallSupportReport {
    pub members: Vec<CxId>,
    pub held_out: Vec<CxId>,
    pub n_queries_tested: usize,
    pub candidate_hits: usize,
}

pub type RecallEvaluationReport = RecallReport;

#[derive(Clone, Debug, PartialEq)]
pub struct InMemoryCorpus {
    name: String,
    queries: Vec<RecallQuery>,
}

impl InMemoryCorpus {
    pub fn new(name: impl Into<String>, queries: Vec<RecallQuery>) -> Self {
        Self {
            name: name.into(),
            queries,
        }
    }
}

impl CorpusReader for InMemoryCorpus {
    fn name(&self) -> &str {
        &self.name
    }

    fn len(&self) -> usize {
        self.queries.len()
    }

    fn query(&self, ordinal: usize) -> Result<RecallQuery> {
        self.queries
            .get(ordinal)
            .cloned()
            .ok_or_else(|| LodestarError::RecallInvalidParams {
                detail: format!("corpus ordinal {ordinal} out of bounds"),
            })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InMemoryAnnIndex {
    rows: Vec<RecallQuery>,
    dim: Option<usize>,
}

impl InMemoryAnnIndex {
    pub fn new(rows: Vec<RecallQuery>) -> Result<Self> {
        let dim = validate_query_rows(&rows)?;
        Ok(Self { rows, dim })
    }
}

impl AnnIndex for InMemoryAnnIndex {
    fn search(&self, query_vec: &[f32], top_k: usize) -> Result<Vec<(CxId, f32)>> {
        validate_query_vec(query_vec, self.dim)?;
        Ok(top_k_by_score(
            self.rows
                .par_iter()
                .map(|row| (row.cx_id, cosine(query_vec, &row.vector)))
                .collect(),
            top_k,
        ))
    }
}

impl AnnIndex for HnswIndex {
    fn search(&self, query_vec: &[f32], top_k: usize) -> Result<Vec<(CxId, f32)>> {
        let dim =
            u32::try_from(query_vec.len()).map_err(|_| LodestarError::RecallInvalidParams {
                detail: format!("query dimension {} exceeds u32::MAX", query_vec.len()),
            })?;
        let hits = SextantIndex::search(
            self,
            &SlotVector::Dense {
                dim,
                data: query_vec.to_vec(),
            },
            top_k,
            None,
        )
        .map_err(|err| LodestarError::KernelIndexBuild {
            detail: err.to_string(),
        })?;
        Ok(hits.into_iter().map(|hit| (hit.cx_id, hit.score)).collect())
    }
}

pub fn measure_kernel_recall(
    kernel_index: &KernelIndex,
    full_index: &dyn AnnIndex,
    corpus: &dyn CorpusReader,
    params: &RecallEvalParams,
) -> Result<RecallEvaluationReport> {
    measure_kernel_recall_with_clock(kernel_index, full_index, corpus, params, &SystemClock)
}

/// Measure kernel-only recall when the corpus similarity is not a dense vector
/// ANN (for example, exact sparse cosine over an inverted index).
///
/// The caller supplies the physical corpus ids and two search functions. Both
/// functions must rank the same association law; comparing different scoring
/// laws would make the recall ratio meaningless.
pub fn measure_ranked_kernel_recall<F, K>(
    corpus_name: &str,
    corpus_ids: &[CxId],
    params: &RecallEvalParams,
    mut full_search: F,
    mut kernel_search: K,
) -> Result<RecallEvaluationReport>
where
    F: FnMut(CxId, usize) -> Result<Vec<(CxId, f32)>>,
    K: FnMut(CxId, usize) -> Result<Vec<(CxId, f32)>>,
{
    validate_params(params)?;
    if corpus_ids.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }
    let seed = if params.rng_seed == 0 {
        SystemClock.now()
    } else {
        params.rng_seed
    };
    let selected = held_out_id_ordinals(corpus_ids, params.held_out_fraction, seed);
    if selected.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }

    let mut held_out = Vec::with_capacity(selected.len());
    let mut total_recall = 0.0_f32;
    for ordinal in selected {
        let query = corpus_ids[ordinal];
        let full_hits = full_search(query, params.top_k)?;
        if full_hits.is_empty() {
            return Err(LodestarError::RecallEmptyCorpus);
        }
        let kernel_hits = kernel_search(query, params.top_k)?;
        total_recall += recall_at_k(&kernel_hits, &full_hits);
        held_out.push(query);
    }
    let kernel_only = total_recall / held_out.len() as f32;
    Ok(RecallEvaluationReport {
        kernel_only,
        full: 1.0,
        ratio: kernel_only,
        approx_factor: 1.0,
        tau_star_estimate: 0,
        tau_star_exact: true,
        recall_eval_params: Some(params.clone()),
        corpus_name: Some(corpus_name.to_string()),
        n_queries_tested: held_out.len(),
        held_out,
        warning: recall_warning(kernel_only, params.min_recall_ratio),
    })
}

pub fn kernel_recall_gate(
    kernel_index: &KernelIndex,
    full_index: &dyn AnnIndex,
    corpus: &dyn CorpusReader,
    params: &RecallEvalParams,
) -> Result<RecallEvaluationReport> {
    kernel_recall_gate_with_clock(kernel_index, full_index, corpus, params, &SystemClock)
}

pub fn kernel_recall_gate_with_clock(
    kernel_index: &KernelIndex,
    full_index: &dyn AnnIndex,
    corpus: &dyn CorpusReader,
    params: &RecallEvalParams,
    clock: &dyn Clock,
) -> Result<RecallEvaluationReport> {
    let report = measure_kernel_recall_with_clock(kernel_index, full_index, corpus, params, clock)?;
    enforce_recall_gate(report, params.min_recall_ratio)
}

pub fn measure_kernel_recall_with_clock(
    kernel_index: &KernelIndex,
    full_index: &dyn AnnIndex,
    corpus: &dyn CorpusReader,
    params: &RecallEvalParams,
    clock: &dyn Clock,
) -> Result<RecallEvaluationReport> {
    validate_params(params)?;
    if corpus.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }

    let seed = if params.rng_seed == 0 {
        clock.now()
    } else {
        params.rng_seed
    };
    let selected = held_out_ordinals(corpus, params.held_out_fraction, seed)?;
    if selected.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }

    let mut held_out = Vec::with_capacity(selected.len());
    let mut total_recall = 0.0_f32;
    for ordinal in selected {
        let query = corpus.query(ordinal)?;
        let full_hits = full_index.search(&query.vector, params.top_k)?;
        if full_hits.is_empty() {
            return Err(LodestarError::RecallEmptyCorpus);
        }
        let kernel_hits = kernel_search(kernel_index, &query.vector, params.top_k)?;
        total_recall += recall_at_k(&kernel_hits, &full_hits);
        held_out.push(query.cx_id);
    }

    let kernel_only = total_recall / held_out.len() as f32;
    let full = 1.0_f32;
    let ratio = kernel_only / full;
    Ok(RecallEvaluationReport {
        kernel_only,
        full,
        ratio,
        approx_factor: 1.0,
        tau_star_estimate: 0,
        tau_star_exact: true,
        recall_eval_params: Some(params.clone()),
        corpus_name: Some(corpus.name().to_string()),
        n_queries_tested: held_out.len(),
        held_out,
        warning: recall_warning(ratio, params.min_recall_ratio),
    })
}

pub fn full_topk_support_set(
    full_index: &dyn AnnIndex,
    corpus: &dyn CorpusReader,
    params: &RecallEvalParams,
) -> Result<RecallSupportReport> {
    validate_params(params)?;
    if corpus.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }
    let selected = held_out_ordinals(corpus, params.held_out_fraction, params.rng_seed)?;
    if selected.is_empty() {
        return Err(LodestarError::RecallEmptyCorpus);
    }

    let mut members = BTreeSet::new();
    let mut held_out = Vec::with_capacity(selected.len());
    let mut candidate_hits = 0_usize;
    for ordinal in selected {
        let query = corpus.query(ordinal)?;
        let full_hits = full_index.search(&query.vector, params.top_k)?;
        if full_hits.is_empty() {
            return Err(LodestarError::RecallEmptyCorpus);
        }
        candidate_hits += full_hits.len();
        members.extend(full_hits.into_iter().map(|(cx_id, _)| cx_id));
        held_out.push(query.cx_id);
    }

    Ok(RecallSupportReport {
        members: members.into_iter().collect(),
        n_queries_tested: held_out.len(),
        held_out,
        candidate_hits,
    })
}

pub fn enforce_recall_gate(
    report: RecallEvaluationReport,
    min_recall_ratio: f32,
) -> Result<RecallEvaluationReport> {
    if report.ratio < min_recall_ratio {
        Err(LodestarError::RecallBelowGate {
            ratio: report.ratio,
            min: min_recall_ratio,
        })
    } else {
        Ok(report)
    }
}

fn validate_params(params: &RecallEvalParams) -> Result<()> {
    if !params.held_out_fraction.is_finite()
        || params.held_out_fraction < 0.0
        || params.held_out_fraction > 1.0
    {
        return Err(LodestarError::RecallInvalidParams {
            detail: "held_out_fraction must be finite and within [0, 1]".to_string(),
        });
    }
    if params.top_k == 0 {
        return Err(LodestarError::RecallInvalidParams {
            detail: "top_k must be greater than zero".to_string(),
        });
    }
    if !params.min_recall_ratio.is_finite()
        || params.min_recall_ratio < 0.0
        || params.min_recall_ratio > 1.0
    {
        return Err(LodestarError::RecallInvalidParams {
            detail: "min_recall_ratio must be finite and within [0, 1]".to_string(),
        });
    }
    Ok(())
}

fn held_out_ordinals(
    corpus: &dyn CorpusReader,
    held_out_fraction: f32,
    seed: u64,
) -> Result<Vec<usize>> {
    let target = ((corpus.len() as f32) * held_out_fraction).ceil() as usize;
    let target = target.min(corpus.len());
    if target == 0 {
        return Ok(Vec::new());
    }

    let mut keyed = Vec::with_capacity(corpus.len());
    for ordinal in 0..corpus.len() {
        let query = corpus.query(ordinal)?;
        keyed.push((sample_key(seed, ordinal, query.cx_id), ordinal));
    }
    keyed.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut selected: Vec<_> = keyed
        .into_iter()
        .take(target)
        .map(|(_, ordinal)| ordinal)
        .collect();
    selected.sort_unstable();
    Ok(selected)
}

fn held_out_id_ordinals(ids: &[CxId], held_out_fraction: f32, seed: u64) -> Vec<usize> {
    let target = ((ids.len() as f32) * held_out_fraction).ceil() as usize;
    let mut keyed = ids
        .iter()
        .enumerate()
        .map(|(ordinal, cx_id)| (sample_key(seed, ordinal, *cx_id), ordinal))
        .collect::<Vec<_>>();
    keyed.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut selected = keyed
        .into_iter()
        .take(target.min(ids.len()))
        .map(|(_, ordinal)| ordinal)
        .collect::<Vec<_>>();
    selected.sort_unstable();
    selected
}

fn sample_key(seed: u64, ordinal: usize, cx_id: CxId) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed.to_be_bytes());
    hasher.update(&(ordinal as u64).to_be_bytes());
    hasher.update(cx_id.as_bytes());
    *hasher.finalize().as_bytes()
}

fn recall_at_k(kernel_hits: &[(CxId, f32)], full_hits: &[(CxId, f32)]) -> f32 {
    let kernel: BTreeSet<_> = kernel_hits.iter().map(|(cx_id, _)| *cx_id).collect();
    let full: BTreeSet<_> = full_hits.iter().map(|(cx_id, _)| *cx_id).collect();
    let overlap = full.iter().filter(|cx_id| kernel.contains(cx_id)).count();
    overlap as f32 / full.len() as f32
}

fn recall_warning(ratio: f32, min_ratio: f32) -> Option<String> {
    (ratio < min_ratio)
        .then(|| format!("{CALYX_KERNEL_RECALL_BELOW_GATE}: ratio={ratio:.6} min={min_ratio:.6}"))
}

fn validate_query_rows(rows: &[RecallQuery]) -> Result<Option<usize>> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let dim = first.vector.len();
    if dim == 0 {
        return Err(LodestarError::RecallInvalidParams {
            detail: "vectors must have non-zero dimension".to_string(),
        });
    }
    for row in rows {
        validate_query_vec(&row.vector, Some(dim))?;
    }
    Ok(Some(dim))
}

fn validate_query_vec(query_vec: &[f32], expected_dim: Option<usize>) -> Result<()> {
    if query_vec.is_empty() {
        return Err(LodestarError::RecallInvalidParams {
            detail: "query vector must have non-zero dimension".to_string(),
        });
    }
    if let Some(expected) = expected_dim
        && query_vec.len() != expected
    {
        return Err(LodestarError::KernelDimMismatch {
            expected,
            actual: query_vec.len(),
        });
    }
    if let Some((offset, _)) = query_vec
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(LodestarError::RecallInvalidParams {
            detail: format!("query vector has non-finite value at offset {offset}"),
        });
    }
    Ok(())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut an = 0.0_f32;
    let mut bn = 0.0_f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        an += x * x;
        bn += y * y;
    }
    if an == 0.0 || bn == 0.0 {
        0.0
    } else {
        dot / (an.sqrt() * bn.sqrt())
    }
}

fn top_k_by_score(mut scored: Vec<(CxId, f32)>, top_k: usize) -> Vec<(CxId, f32)> {
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
    });
    scored.truncate(top_k);
    scored
}
