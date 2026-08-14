use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use calyx_core::{CxId, SlotId, SlotVector, SparseEntry};
use calyx_sextant::index::bm25::Bm25;
use calyx_sextant::index::{IndexSearchHit, ranked};
use serde::{Deserialize, Serialize};

use super::fs_io::HashingReader;
use super::{SearchIndexEntry, stale};
use crate::error::CliResult;

#[path = "sparse/writer.rs"]
mod writer;
pub(in crate::persisted) use writer::StreamingWriter;

const SPARSE_FORMAT_V4: &str = "calyx-search-sparse-index-v4-jsonl";
const MAX_SPARSE_LINE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SparseScoring {
    Bm25,
    DotProduct,
}

impl SparseScoring {
    pub(super) const fn index_kind(self) -> &'static str {
        match self {
            Self::Bm25 => "sparse_bm25",
            Self::DotProduct => "sparse_dot",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StreamingSparseHeader {
    format: String,
    scoring: SparseScoring,
    slot: u16,
    dim: u32,
    base_seq: u64,
    len: usize,
    field_docs: usize,
    avg_doc_len: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SparseRow {
    cx_id: CxId,
    doc_len: f32,
    entries: Vec<SparseEntry>,
}

pub(super) fn search(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<IndexSearchHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let SlotVector::Sparse {
        dim: query_dim,
        entries,
    } = query
    else {
        return Err(stale(format!(
            "persistent sparse search slot {slot} received non-sparse query"
        )));
    };
    query.validate_schema().map_err(|err| {
        stale(format!(
            "persistent sparse search slot {slot} received invalid query: {}",
            err.message
        ))
    })?;
    require_streaming_sidecar(entry, slot)?;
    let scored = score_streaming(SparseSearchRequest {
        vault_dir,
        entry,
        manifest_base_seq,
        slot,
        query_dim: *query_dim,
        query: entries,
        candidates,
        k,
    })?;
    Ok(ranked(scored))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search_reconciled(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
    candidates: Option<&BTreeSet<CxId>>,
    changed: &BTreeSet<CxId>,
    replacements: &BTreeMap<CxId, SlotVector>,
) -> CliResult<Vec<IndexSearchHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let SlotVector::Sparse {
        dim: query_dim,
        entries: query_entries,
    } = query
    else {
        return Err(stale(format!(
            "sparse delta reconciliation for slot {slot} received non-sparse query"
        )));
    };
    query.validate_schema().map_err(|error| {
        stale(format!(
            "sparse delta query for slot {slot} is invalid: {}",
            error.message
        ))
    })?;
    require_streaming_sidecar(entry, slot)?;
    let scored = score_streaming_reconciled(
        SparseSearchRequest {
            vault_dir,
            entry,
            manifest_base_seq,
            slot,
            query_dim: *query_dim,
            query: query_entries,
            candidates,
            k,
        },
        changed,
        replacements,
    )?;
    Ok(ranked(scored))
}

fn sparse_replacement_rows(
    slot: SlotId,
    expected_dim: u32,
    scoring: SparseScoring,
    replacements: &BTreeMap<CxId, SlotVector>,
) -> CliResult<BTreeMap<CxId, SparseRow>> {
    replacements
        .iter()
        .map(|(cx_id, vector)| {
            let SlotVector::Sparse { dim, entries } = vector else {
                return Err(stale(format!(
                    "changed row {cx_id} for sparse slot {slot} has a non-sparse vector"
                )));
            };
            if *dim != expected_dim {
                return Err(stale(format!(
                    "changed row {cx_id} for sparse slot {slot} has dim {dim}, expected {expected_dim}"
                )));
            }
            vector.validate_schema().map_err(|error| {
                stale(format!(
                    "changed row {cx_id} for sparse slot {slot} is invalid: {}",
                    error.message
                ))
            })?;
            let doc_len = validate_sparse_weights(entries, scoring, &format!("delta row {cx_id}"))?;
            Ok((
                *cx_id,
                SparseRow {
                    cx_id: *cx_id,
                    doc_len,
                    entries: entries.clone(),
                },
            ))
        })
        .collect()
}

struct SparseSearchRequest<'a> {
    vault_dir: &'a Path,
    entry: &'a SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query_dim: u32,
    query: &'a [SparseEntry],
    candidates: Option<&'a BTreeSet<CxId>>,
    k: usize,
}

fn score_streaming(request: SparseSearchRequest<'_>) -> CliResult<Vec<(CxId, f32)>> {
    let SparseSearchRequest {
        vault_dir,
        entry,
        manifest_base_seq,
        slot,
        query_dim,
        query,
        candidates,
        k,
    } = request;
    let mut scoring = None;
    let mut dot_scores = Vec::new();
    let mut document_frequencies = BTreeMap::<u32, usize>::new();
    let header = visit_stream(vault_dir, entry, manifest_base_seq, slot, |header, row| {
        if scoring.is_none() {
            validate_sparse_query_weights(query, header.scoring, "query")?;
            scoring = Some(header.scoring);
        }
        if header.scoring == SparseScoring::DotProduct {
            score_streaming_dot_row(row, query, candidates, &mut dot_scores, k)?;
        } else {
            count_query_terms(row, query, &mut document_frequencies);
        }
        Ok(())
    })?;
    if header.dim != query_dim {
        return Err(stale(format!(
            "persistent streaming sparse slot {slot} index dim {} != query dim {query_dim}; reingest/backfill the vault",
            header.dim
        )));
    }
    validate_sparse_query_weights(query, header.scoring, "query")?;
    if header.scoring == SparseScoring::DotProduct {
        return Ok(top_k(dot_scores, k));
    }
    let mut scores = Vec::new();
    visit_stream(vault_dir, entry, manifest_base_seq, slot, |_, row| {
        score_streaming_bm25_row(
            row,
            query,
            candidates,
            header.field_docs,
            header.avg_doc_len,
            &document_frequencies,
            &mut scores,
            k,
        )
    })?;
    Ok(top_k(scores, k))
}

fn score_streaming_reconciled(
    request: SparseSearchRequest<'_>,
    changed: &BTreeSet<CxId>,
    replacements: &BTreeMap<CxId, SlotVector>,
) -> CliResult<Vec<(CxId, f32)>> {
    let SparseSearchRequest {
        vault_dir,
        entry,
        manifest_base_seq,
        slot,
        query_dim,
        query,
        candidates,
        k,
    } = request;
    let mut scoring = None;
    let mut dot_scores = Vec::new();
    let mut document_frequencies = BTreeMap::<u32, usize>::new();
    let mut unchanged_field_docs = 0usize;
    let mut unchanged_total_doc_len = 0.0_f32;
    let header = visit_stream(vault_dir, entry, manifest_base_seq, slot, |header, row| {
        if scoring.is_none() {
            validate_sparse_query_weights(query, header.scoring, "delta query")?;
            scoring = Some(header.scoring);
        }
        if changed.contains(&row.cx_id) {
            return Ok(());
        }
        if header.scoring == SparseScoring::DotProduct {
            score_streaming_dot_row(row, query, candidates, &mut dot_scores, k)?;
        } else if !row.entries.is_empty() {
            unchanged_field_docs = unchanged_field_docs
                .checked_add(1)
                .ok_or_else(|| stale("reconciled sparse field-document count overflow"))?;
            unchanged_total_doc_len += row.doc_len;
            if !unchanged_total_doc_len.is_finite() {
                return Err(stale("reconciled sparse corpus length overflowed"));
            }
            count_query_terms(row, query, &mut document_frequencies);
        }
        Ok(())
    })?;
    if header.dim != query_dim {
        return Err(stale(format!(
            "persistent streaming sparse slot {slot} index dim {} != delta query dim {query_dim}",
            header.dim
        )));
    }
    validate_sparse_query_weights(query, header.scoring, "delta query")?;
    let replacement_rows = sparse_replacement_rows(slot, header.dim, header.scoring, replacements)?;
    if header.scoring == SparseScoring::DotProduct {
        for row in replacement_rows.values() {
            score_streaming_dot_row(row, query, candidates, &mut dot_scores, k)?;
        }
        return Ok(top_k(dot_scores, k));
    }
    let mut total_docs = unchanged_field_docs;
    let mut total_doc_len = unchanged_total_doc_len;
    for row in replacement_rows
        .values()
        .filter(|row| !row.entries.is_empty())
    {
        total_docs = total_docs
            .checked_add(1)
            .ok_or_else(|| stale("reconciled sparse field-document count overflow"))?;
        total_doc_len += row.doc_len;
        if !total_doc_len.is_finite() {
            return Err(stale("reconciled sparse corpus length overflowed"));
        }
        count_query_terms(row, query, &mut document_frequencies);
    }
    let avg_doc_len = if total_docs == 0 {
        0.0
    } else {
        total_doc_len / total_docs as f32
    };
    let mut scores = Vec::new();
    visit_stream(vault_dir, entry, manifest_base_seq, slot, |_, row| {
        if changed.contains(&row.cx_id) {
            return Ok(());
        }
        score_streaming_bm25_row(
            row,
            query,
            candidates,
            total_docs,
            avg_doc_len,
            &document_frequencies,
            &mut scores,
            k,
        )
    })?;
    for row in replacement_rows.values() {
        score_streaming_bm25_row(
            row,
            query,
            candidates,
            total_docs,
            avg_doc_len,
            &document_frequencies,
            &mut scores,
            k,
        )?;
    }
    Ok(top_k(scores, k))
}

fn count_query_terms(
    row: &SparseRow,
    query: &[SparseEntry],
    document_frequencies: &mut BTreeMap<u32, usize>,
) {
    for query_entry in query {
        if row
            .entries
            .binary_search_by_key(&query_entry.idx, |entry| entry.idx)
            .is_ok()
        {
            *document_frequencies.entry(query_entry.idx).or_default() += 1;
        }
    }
}

fn score_streaming_dot_row(
    row: &SparseRow,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
    scores: &mut Vec<(CxId, f32)>,
    k: usize,
) -> CliResult {
    if candidates.is_some_and(|allowed| !allowed.contains(&row.cx_id)) {
        return Ok(());
    }
    let mut score = 0.0_f32;
    for query_entry in query {
        if let Ok(index) = row
            .entries
            .binary_search_by_key(&query_entry.idx, |entry| entry.idx)
        {
            score += row.entries[index].val * query_entry.val;
        }
    }
    if !score.is_finite() {
        return Err(stale(format!(
            "persistent streaming sparse dot-product score overflowed for {}",
            row.cx_id
        )));
    }
    if score != 0.0 && k != 0 {
        scores.push((row.cx_id, score));
        prune_score_window(scores, k);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn score_streaming_bm25_row(
    row: &SparseRow,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
    total_docs: usize,
    avg_doc_len: f32,
    document_frequencies: &BTreeMap<u32, usize>,
    scores: &mut Vec<(CxId, f32)>,
    k: usize,
) -> CliResult {
    if candidates.is_some_and(|allowed| !allowed.contains(&row.cx_id)) {
        return Ok(());
    }
    let scorer = Bm25::default();
    let mut score = 0.0_f32;
    for query_entry in query {
        let Ok(index) = row
            .entries
            .binary_search_by_key(&query_entry.idx, |entry| entry.idx)
        else {
            continue;
        };
        let df = document_frequencies
            .get(&query_entry.idx)
            .copied()
            .unwrap_or(0);
        score += scorer.score_term(
            row.entries[index].val,
            row.doc_len,
            avg_doc_len,
            total_docs,
            df,
        ) * query_entry.val;
    }
    if !score.is_finite() {
        return Err(stale(format!(
            "persistent streaming sparse BM25 score overflowed for {}",
            row.cx_id
        )));
    }
    if score != 0.0 && k != 0 {
        scores.push((row.cx_id, score));
        prune_score_window(scores, k);
    }
    Ok(())
}

fn visit_stream<F>(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    mut visit: F,
) -> CliResult<StreamingSparseHeader>
where
    F: FnMut(&StreamingSparseHeader, &SparseRow) -> CliResult,
{
    require_streaming_sidecar(entry, slot)?;
    require_sparse_kind(entry, slot)?;
    let path = vault_dir.join(entry.require_index_rel(slot)?);
    if !path.is_file() {
        return Err(stale(format!(
            "persistent streaming sparse sidecar missing at {}; rebuild the vault search indexes",
            path.display()
        )));
    }
    let mut hashing_reader = HashingReader::new(File::open(&path)?);
    let mut reader = BufReader::new(&mut hashing_reader);
    let header_line = read_bounded_line(&mut reader, &path, "header")?.ok_or_else(|| {
        stale(format!(
            "persistent streaming sparse sidecar {} is empty",
            path.display()
        ))
    })?;
    let header: StreamingSparseHeader = serde_json::from_str(&header_line).map_err(|error| {
        stale(format!(
            "persistent streaming sparse header {} is invalid JSON: {error}",
            path.display()
        ))
    })?;
    validate_streaming_header(&header, entry, manifest_base_seq, slot)?;
    let mut count = 0usize;
    let mut field_docs = 0usize;
    let mut total_doc_len = 0.0_f32;
    let mut last_cx_id = None;
    while let Some(line) = read_bounded_line(&mut reader, &path, "row")? {
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            return Err(stale(format!(
                "persistent streaming sparse sidecar {} contains an empty row line",
                path.display()
            )));
        }
        let row: SparseRow = serde_json::from_str(&line).map_err(|error| {
            stale(format!(
                "persistent streaming sparse row {} is invalid JSON: {error}",
                path.display()
            ))
        })?;
        if last_cx_id.is_some_and(|previous| previous >= row.cx_id) {
            return Err(stale(format!(
                "persistent streaming sparse rows are not strictly ordered: prior {last_cx_id:?}, current {}",
                row.cx_id
            )));
        }
        last_cx_id = Some(row.cx_id);
        let expected_doc_len =
            validate_sparse_weights(&row.entries, header.scoring, &format!("row {}", row.cx_id))?;
        if row.doc_len.to_bits() != expected_doc_len.to_bits() {
            return Err(stale(format!(
                "persistent streaming sparse row {} doc_len {} != weight sum {expected_doc_len}",
                row.cx_id, row.doc_len
            )));
        }
        SlotVector::Sparse {
            dim: header.dim,
            entries: row.entries.clone(),
        }
        .validate_schema()
        .map_err(|error| {
            stale(format!(
                "persistent streaming sparse row {} has invalid payload: {}",
                row.cx_id, error.message
            ))
        })?;
        if !row.entries.is_empty() {
            field_docs = field_docs
                .checked_add(1)
                .ok_or_else(|| stale("streaming sparse field-document count overflow"))?;
            total_doc_len += row.doc_len;
            if !total_doc_len.is_finite() {
                return Err(stale("streaming sparse corpus length overflowed"));
            }
        }
        visit(&header, &row)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| stale("streaming sparse row count overflow"))?;
    }
    if count != header.len || count != entry.len {
        return Err(stale(format!(
            "persistent streaming sparse row len {count} != header len {} / manifest len {}",
            header.len, entry.len
        )));
    }
    let avg_doc_len = if field_docs == 0 {
        0.0
    } else {
        total_doc_len / field_docs as f32
    };
    if field_docs != header.field_docs || (avg_doc_len - header.avg_doc_len).abs() > f32::EPSILON {
        return Err(stale(format!(
            "persistent streaming sparse corpus stats field_docs={field_docs} avg_doc_len={avg_doc_len} do not match header field_docs={} avg_doc_len={}",
            header.field_docs, header.avg_doc_len
        )));
    }
    drop(reader);
    let actual = hashing_reader.into_sha256();
    let expected = entry.require_sha256(slot)?;
    if actual != expected {
        return Err(stale(format!(
            "persistent streaming sparse sidecar sha256 {actual} != manifest {expected}; rebuild the vault search indexes"
        )));
    }
    Ok(header)
}

fn validate_streaming_header(
    header: &StreamingSparseHeader,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult {
    if header.format != SPARSE_FORMAT_V4 {
        return Err(stale(format!(
            "persistent streaming sparse format {} != {SPARSE_FORMAT_V4}",
            header.format
        )));
    }
    entry.require_kind(header.scoring.index_kind(), slot)?;
    if header.slot != slot.get() || entry.slot != slot.get() {
        return Err(stale(format!(
            "persistent streaming sparse slot {} / entry slot {} != query slot {}",
            header.slot,
            entry.slot,
            slot.get()
        )));
    }
    let entry_dim = entry.require_dim(slot)?;
    if header.dim != entry_dim {
        return Err(stale(format!(
            "persistent streaming sparse dim {} != manifest dim {entry_dim}",
            header.dim
        )));
    }
    if header.base_seq != manifest_base_seq || entry.built_at_seq != manifest_base_seq {
        return Err(stale(format!(
            "persistent streaming sparse seq {} / entry seq {} != manifest seq {manifest_base_seq}",
            header.base_seq, entry.built_at_seq
        )));
    }
    if header.len != entry.len || !header.avg_doc_len.is_finite() || header.avg_doc_len < 0.0 {
        return Err(stale(format!(
            "persistent streaming sparse header len {} / avg_doc_len {} is inconsistent with manifest len {}",
            header.len, header.avg_doc_len, entry.len
        )));
    }
    Ok(())
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    path: &Path,
    kind: &str,
) -> CliResult<Option<String>> {
    let mut line = String::new();
    let read = reader
        .take(MAX_SPARSE_LINE_BYTES + 1)
        .read_line(&mut line)
        .map_err(|error| {
            stale(format!(
                "persistent streaming sparse {kind} at {} could not be decoded as UTF-8: {error}",
                path.display()
            ))
        })?;
    if read == 0 {
        return Ok(None);
    }
    if read as u64 > MAX_SPARSE_LINE_BYTES {
        return Err(stale(format!(
            "persistent streaming sparse {kind} at {} exceeds the {MAX_SPARSE_LINE_BYTES}-byte structural bound",
            path.display()
        )));
    }
    if !line.ends_with('\n') {
        return Err(stale(format!(
            "persistent streaming sparse {kind} at {} is not newline-terminated",
            path.display()
        )));
    }
    Ok(Some(line))
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult {
    require_streaming_sidecar(entry, slot)?;
    visit_stream(vault_dir, entry, manifest_base_seq, slot, |_, _| Ok(()))?;
    Ok(())
}

/// Whether this call is validating a stored document row or a query vector.
///
/// The distinction matters for BM25 and only for BM25. `tf_td` — the document
/// side — is *defined* as a raw occurrence count, and the whole `b`/`avgdl`
/// length correction is a statement about counts summing to a document length.
/// `qtf_t` — the query side — is a weight, and a non-integral query weight is a
/// legitimate boost rather than a broken measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SparseWeightRole {
    /// A stored row's measured vector. BM25 requires raw counts here.
    Document,
    /// A query vector. Weights are boosts and need not be counts.
    Query,
}

fn validate_sparse_weights(
    entries: &[SparseEntry],
    scoring: SparseScoring,
    context: &str,
) -> CliResult<f32> {
    validate_sparse_weights_for(entries, scoring, SparseWeightRole::Document, context)
}

fn validate_sparse_query_weights(
    entries: &[SparseEntry],
    scoring: SparseScoring,
    context: &str,
) -> CliResult<f32> {
    validate_sparse_weights_for(entries, scoring, SparseWeightRole::Query, context)
}

fn validate_sparse_weights_for(
    entries: &[SparseEntry],
    scoring: SparseScoring,
    role: SparseWeightRole,
    context: &str,
) -> CliResult<f32> {
    let mut total = 0.0_f32;
    for entry in entries {
        // A BM25 document weight must be a raw term-frequency COUNT.
        //
        // This is not stylistic strictness. BM25's document-length saturation
        // reads `doc_len` as the sum of a row's weights and compares it against
        // the corpus average. An encoder that L1-normalizes its output makes
        // every row sum to exactly 1.0, so `len_norm` is 1.0 for every
        // document, `b` and `avgdl` are computed and persisted and validated
        // and cannot move a single score, and the `b=0.75` the manifest implies
        // is operationally `b=0` (#1902). The identical defect one lane over
        // was #1900.
        //
        // "Every weight is a positive integer" is the checkable form of
        // "these are counts", and it cannot false-positive on a genuine count
        // lane. A normalized lane fails it on the first row carrying two
        // distinct terms.
        if scoring == SparseScoring::Bm25
            && role == SparseWeightRole::Document
            && entry.val.is_finite()
            && entry.val > 0.0
            && entry.val.fract() != 0.0
        {
            return Err(stale(format!(
                "persistent sparse BM25 {context} weight {} at index {} is not a whole term-frequency count; BM25 scores a raw count and derives the document length from the weight sum, so a normalized or otherwise fractional lane has a length correction that cannot act (b/avgdl become inert). Score this lens as sparse_dot, or measure it with a raw-count encoder (for example syn_sparse_text_tf / sparse_keywords_tf) and rebuild the vault search indexes",
                entry.val, entry.idx
            )));
        }
        match scoring {
            SparseScoring::Bm25 if !entry.val.is_finite() || entry.val <= 0.0 => {
                return Err(stale(format!(
                    "persistent sparse BM25 {context} weight at index {} must be finite and greater than zero",
                    entry.idx
                )));
            }
            SparseScoring::DotProduct if !entry.val.is_finite() || entry.val == 0.0 => {
                return Err(stale(format!(
                    "persistent sparse dot-product {context} weight at index {} must be finite and non-zero",
                    entry.idx
                )));
            }
            _ => {}
        }
        total += match scoring {
            SparseScoring::Bm25 => entry.val,
            SparseScoring::DotProduct => entry.val.abs(),
        };
        if !total.is_finite() {
            return Err(stale(format!(
                "persistent sparse {context} weight sum overflowed"
            )));
        }
    }
    Ok(total)
}

pub(super) fn require_sparse_kind(entry: &SearchIndexEntry, slot: SlotId) -> CliResult {
    match entry.kind.as_str() {
        "sparse_bm25" | "sparse_dot" => Ok(()),
        other => Err(stale(format!(
            "persistent slot {slot} index kind {other} is not a supported sparse index; rebuild the vault search indexes"
        ))),
    }
}

fn require_streaming_sidecar(entry: &SearchIndexEntry, slot: SlotId) -> CliResult {
    let index_rel = entry.require_index_rel(slot)?;
    if !index_rel.ends_with(".sparse.jsonl") {
        return Err(stale(format!(
            "persistent sparse slot {slot} uses legacy sidecar {index_rel}; legacy whole-corpus sparse indexes are not loaded because their decoded postings have unbounded process lifetime. Rebuild the vault search indexes into {SPARSE_FORMAT_V4}"
        )));
    }
    Ok(())
}

fn top_k(mut scored: Vec<(CxId, f32)>, k: usize) -> Vec<(CxId, f32)> {
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
    });
    scored.truncate(k);
    scored
}

fn prune_score_window(scored: &mut Vec<(CxId, f32)>, k: usize) {
    let prune_at = k.saturating_mul(2).max(k.saturating_add(1));
    if scored.len() >= prune_at {
        *scored = top_k(std::mem::take(scored), k);
    }
}
