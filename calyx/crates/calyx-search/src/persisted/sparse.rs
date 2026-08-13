use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use calyx_core::{CxId, SlotId, SlotVector, SparseEntry};
use calyx_sextant::index::bm25::Bm25;
use calyx_sextant::index::{IndexSearchHit, ranked};
use serde::{Deserialize, Serialize};

use super::fs_io::HashingReader;
use super::pinned::{self, PinKey};
use super::{SearchIndexEntry, rel, sha256_hex, stale, write_atomic_hashed};
use crate::error::CliResult;

const SPARSE_FORMAT_V2: &str = "calyx-search-sparse-index-v2";
const SPARSE_FORMAT_V3: &str = "calyx-search-sparse-index-v3";
const SPARSE_FORMAT_V4: &str = "calyx-search-sparse-index-v4-jsonl";
const MAX_SPARSE_LINE_BYTES: u64 = 64 * 1024 * 1024;
const LEGACY_BM25_KIND: &str = "sparse_inverted";
const PIN_KIND: &str = "sparse";

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

const fn legacy_sparse_scoring() -> SparseScoring {
    SparseScoring::Bm25
}

#[derive(Clone, Debug)]
pub(super) struct SparseSlotRows {
    pub(super) dim: u32,
    pub(super) rows: Vec<(CxId, Vec<SparseEntry>)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SparseIndex {
    format: String,
    #[serde(default = "legacy_sparse_scoring")]
    scoring: SparseScoring,
    slot: u16,
    dim: u32,
    base_seq: u64,
    rows: Vec<SparseRow>,
    postings: BTreeMap<u32, Vec<SparsePosting>>,
    doc_lengths: BTreeMap<CxId, f32>,
    avg_doc_len: f32,
    /// Rows that carry at least one term in this lane.
    ///
    /// **Not persisted**, and deliberately so: it is derivable from `rows` in one
    /// pass at load time, and a stored copy could disagree with the payload it
    /// summarizes. Populated by `build_index` and by `read` after
    /// deserialization; `SparseIndex` is constructed nowhere else.
    ///
    /// This is BM25's `N` (and the denominator of `avg_doc_len`), which must
    /// count documents that *have the field* rather than every row in the panel.
    /// On the live timeline panel 238 of 339 rows carry no title at all, so
    /// averaging over all rows put `avg_doc_len` at 0.2956 instead of 0.992 —
    /// making every title-bearing document look 3x longer than average and
    /// re-introducing, through the `b` term, exactly the short-document bias `b`
    /// exists to remove. Lucene scopes both to the field's `docCount` for the
    /// same reason (#1900).
    #[serde(skip)]
    field_docs: usize,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SparsePosting {
    cx_id: CxId,
    tf: f32,
}

impl SparseSlotRows {
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
}

pub(super) fn write<F>(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    rows: SparseSlotRows,
    base_seq: u64,
    scoring: SparseScoring,
    mut progress: F,
) -> CliResult<SearchIndexEntry>
where
    F: FnMut(&'static str, usize) -> CliResult,
{
    let row_count = rows.rows.len();
    let path = root.join(format!(
        "slot_{:05}_seq_{base_seq:020}_n_{row_count:010}.sparse.jsonl",
        slot.get(),
    ));
    let mut validated_rows = Vec::with_capacity(row_count);
    let mut total_doc_len = 0.0_f32;
    let mut field_docs = 0usize;
    for (cx_id, entries) in rows.rows {
        let doc_len = validate_sparse_weights(&entries, scoring, &format!("row {cx_id}"))?;
        if !entries.is_empty() {
            field_docs = field_docs
                .checked_add(1)
                .ok_or_else(|| stale("sparse field-document count overflow"))?;
            total_doc_len += doc_len;
            if !total_doc_len.is_finite() {
                return Err(stale("persistent sparse corpus length overflowed"));
            }
        }
        validated_rows.push(SparseRow {
            cx_id,
            doc_len,
            entries,
        });
    }
    let avg_doc_len = if field_docs == 0 {
        0.0
    } else {
        total_doc_len / field_docs as f32
    };
    let header = StreamingSparseHeader {
        format: SPARSE_FORMAT_V4.to_owned(),
        scoring,
        slot: slot.get(),
        dim: rows.dim,
        base_seq,
        len: row_count,
        field_docs,
        avg_doc_len,
    };
    progress("slot.sparse.rows_validated", row_count)?;
    let sha256 = write_atomic_hashed(&path, |writer| {
        serde_json::to_writer(&mut *writer, &header)?;
        writer.write_all(b"\n")?;
        for row in &validated_rows {
            serde_json::to_writer(&mut *writer, row)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    })?;
    Ok(SearchIndexEntry::sparse(
        slot,
        header.dim,
        row_count,
        base_seq,
        rel(vault_dir, &path)?,
        sha256,
        scoring.index_kind(),
    ))
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
    if entry.require_index_rel(slot)?.ends_with(".sparse.jsonl") {
        let scored = score_streaming(
            vault_dir,
            entry,
            manifest_base_seq,
            slot,
            *query_dim,
            entries,
            candidates,
        )?;
        return Ok(ranked(top_k(scored, k)));
    }
    let index = pinned_index(vault_dir, entry, manifest_base_seq, slot)?;
    validate_sparse_query_weights(entries, index.scoring, "query")?;
    if index.dim != *query_dim {
        return Err(stale(format!(
            "persistent sparse slot {slot} index dim {} != query dim {query_dim}; reingest/backfill the vault",
            index.dim
        )));
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    Ok(ranked(top_k(score(&index, entries, candidates)?, k)))
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
    if entry.require_index_rel(slot)?.ends_with(".sparse.jsonl") {
        let scored = score_streaming_reconciled(
            vault_dir,
            entry,
            manifest_base_seq,
            slot,
            *query_dim,
            query_entries,
            candidates,
            changed,
            replacements,
        )?;
        return Ok(ranked(top_k(scored, k)));
    }
    let index = pinned_index(vault_dir, entry, manifest_base_seq, slot)?;
    if index.dim != *query_dim {
        return Err(stale(format!(
            "persistent sparse slot {slot} index dim {} != delta query dim {query_dim}",
            index.dim
        )));
    }
    validate_sparse_query_weights(query_entries, index.scoring, "delta query")?;
    let replacement_rows = sparse_replacement_rows(slot, *query_dim, index.scoring, replacements)?;
    let scored = match index.scoring {
        SparseScoring::DotProduct => score_dot_reconciled(
            &index,
            query_entries,
            candidates,
            changed,
            &replacement_rows,
        )?,
        SparseScoring::Bm25 => score_bm25_reconciled(
            &index,
            query_entries,
            candidates,
            changed,
            &replacement_rows,
        )?,
    };
    Ok(ranked(top_k(scored, k)))
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

fn score_dot_reconciled(
    index: &SparseIndex,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
    changed: &BTreeSet<CxId>,
    replacements: &BTreeMap<CxId, SparseRow>,
) -> CliResult<Vec<(CxId, f32)>> {
    let mut scores = score_dot_product(index, query, candidates)?
        .into_iter()
        .filter(|(cx_id, _)| !changed.contains(cx_id))
        .collect::<BTreeMap<_, _>>();
    for row in replacements.values() {
        if candidates.is_some_and(|allowed| !allowed.contains(&row.cx_id)) {
            continue;
        }
        let mut score = 0.0_f32;
        for query_entry in query {
            if let Some(entry) = row
                .entries
                .iter()
                .find(|entry| entry.idx == query_entry.idx)
            {
                score += entry.val * query_entry.val;
            }
        }
        if !score.is_finite() {
            return Err(stale(format!(
                "reconciled sparse dot-product score overflowed for {}",
                row.cx_id
            )));
        }
        if score != 0.0 {
            scores.insert(row.cx_id, score);
        }
    }
    Ok(scores.into_iter().collect())
}

fn score_bm25_reconciled(
    index: &SparseIndex,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
    changed: &BTreeSet<CxId>,
    replacements: &BTreeMap<CxId, SparseRow>,
) -> CliResult<Vec<(CxId, f32)>> {
    let removed = index
        .rows
        .iter()
        .filter(|row| changed.contains(&row.cx_id))
        .collect::<Vec<_>>();
    // Field-bearing counts on both sides of the delta, for the same reason the
    // static path uses `field_docs`: a removed or replacing row that carries no
    // term in this lane is not a document of this field.
    let removed_field_docs = removed.iter().filter(|row| !row.entries.is_empty()).count();
    let replacement_field_docs = replacements
        .values()
        .filter(|row| !row.entries.is_empty())
        .count();
    let total_docs = index
        .field_docs
        .saturating_sub(removed_field_docs)
        .saturating_add(replacement_field_docs);
    let old_total = index.doc_lengths.values().copied().sum::<f32>();
    let removed_total = removed.iter().map(|row| row.doc_len).sum::<f32>();
    let replacement_total = replacements.values().map(|row| row.doc_len).sum::<f32>();
    let current_total = old_total - removed_total + replacement_total;
    if !current_total.is_finite() || current_total < 0.0 {
        return Err(stale(
            "reconciled sparse BM25 corpus length is invalid; rebuild the search generation",
        ));
    }
    let avg_doc_len = if total_docs == 0 {
        0.0
    } else {
        current_total / total_docs as f32
    };
    let scorer = Bm25::default();
    let mut scores = BTreeMap::<CxId, f32>::new();
    for query_entry in query {
        let old_postings = index.postings.get(&query_entry.idx);
        let removed_df = removed
            .iter()
            .filter(|row| row.entries.iter().any(|entry| entry.idx == query_entry.idx))
            .count();
        let replacement_df = replacements
            .values()
            .filter(|row| row.entries.iter().any(|entry| entry.idx == query_entry.idx))
            .count();
        let df = old_postings
            .map_or(0, Vec::len)
            .saturating_sub(removed_df)
            .saturating_add(replacement_df);
        if let Some(postings) = old_postings {
            for posting in postings {
                if changed.contains(&posting.cx_id)
                    || candidates.is_some_and(|allowed| !allowed.contains(&posting.cx_id))
                {
                    continue;
                }
                let len = *index.doc_lengths.get(&posting.cx_id).unwrap_or(&1.0);
                *scores.entry(posting.cx_id).or_default() +=
                    scorer.score_term(posting.tf, len, avg_doc_len, total_docs, df)
                        * query_entry.val;
            }
        }
        for row in replacements.values() {
            if candidates.is_some_and(|allowed| !allowed.contains(&row.cx_id)) {
                continue;
            }
            if let Some(entry) = row
                .entries
                .iter()
                .find(|entry| entry.idx == query_entry.idx)
            {
                *scores.entry(row.cx_id).or_default() +=
                    scorer.score_term(entry.val, row.doc_len, avg_doc_len, total_docs, df)
                        * query_entry.val;
            }
        }
    }
    if let Some((cx_id, _)) = scores.iter().find(|(_, score)| !score.is_finite()) {
        return Err(stale(format!(
            "reconciled sparse BM25 score overflowed for {cx_id}"
        )));
    }
    Ok(scores.into_iter().collect())
}

fn score_streaming(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query_dim: u32,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<(CxId, f32)>> {
    let mut scoring = None;
    let mut dot_scores = BTreeMap::new();
    let mut document_frequencies = BTreeMap::<u32, usize>::new();
    let header = visit_stream(vault_dir, entry, manifest_base_seq, slot, |header, row| {
        if scoring.is_none() {
            validate_sparse_query_weights(query, header.scoring, "query")?;
            scoring = Some(header.scoring);
        }
        if header.scoring == SparseScoring::DotProduct {
            score_streaming_dot_row(row, query, candidates, &mut dot_scores)?;
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
        return Ok(dot_scores.into_iter().collect());
    }
    let mut scores = BTreeMap::new();
    visit_stream(vault_dir, entry, manifest_base_seq, slot, |_, row| {
        score_streaming_bm25_row(
            row,
            query,
            candidates,
            header.field_docs,
            header.avg_doc_len,
            &document_frequencies,
            &mut scores,
        )
    })?;
    Ok(scores.into_iter().collect())
}

#[allow(clippy::too_many_arguments)]
fn score_streaming_reconciled(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query_dim: u32,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
    changed: &BTreeSet<CxId>,
    replacements: &BTreeMap<CxId, SlotVector>,
) -> CliResult<Vec<(CxId, f32)>> {
    let mut scoring = None;
    let mut dot_scores = BTreeMap::new();
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
            score_streaming_dot_row(row, query, candidates, &mut dot_scores)?;
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
            score_streaming_dot_row(row, query, candidates, &mut dot_scores)?;
        }
        return Ok(dot_scores.into_iter().collect());
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
    let mut scores = BTreeMap::new();
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
        )?;
    }
    Ok(scores.into_iter().collect())
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
    scores: &mut BTreeMap<CxId, f32>,
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
    if score != 0.0 {
        scores.insert(row.cx_id, score);
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
    scores: &mut BTreeMap<CxId, f32>,
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
    if score != 0.0 {
        scores.insert(row.cx_id, score);
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

type SparsePinCache = Mutex<BTreeMap<(String, u16), (String, Arc<SparseIndex>)>>;

fn cache() -> &'static SparsePinCache {
    static CACHE: OnceLock<SparsePinCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Verify-once-then-pin: the sparse sidecar is fully read, hashed, and
/// structurally validated on first use per manifest generation (keyed by the
/// manifest entry sha256); cache hits still fail closed on any seq drift
/// between the pinned index and the manifest being served.
fn pinned_index(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult<Arc<SparseIndex>> {
    let entry_sha256 = entry.require_sha256(slot)?.to_string();
    let cache_key = (pinned::canonical_vault_dir(vault_dir)?, slot.get());
    {
        let cache = cache().lock().expect("sparse pin cache poisoned");
        if let Some((pinned_sha, index)) = cache.get(&cache_key)
            && *pinned_sha == entry_sha256
        {
            if index.base_seq != manifest_base_seq || entry.built_at_seq != manifest_base_seq {
                return Err(stale(format!(
                    "persistent sparse sidecar seq {} / entry seq {} != manifest seq {manifest_base_seq}; rebuild the vault search indexes",
                    index.base_seq, entry.built_at_seq
                )));
            }
            return Ok(Arc::clone(index));
        }
    }
    let path = vault_dir.join(entry.require_index_rel(slot)?);
    let sidecar_bytes = if path.is_file() {
        fs::metadata(&path)?.len()
    } else {
        0
    };
    let index = Arc::new(read(vault_dir, entry, manifest_base_seq, slot)?);
    let pin_key = PinKey::new(vault_dir, slot.get(), PIN_KIND)?;
    pinned::reserve(&pin_key, sidecar_bytes)?;
    let mut cache = cache().lock().expect("sparse pin cache poisoned");
    cache.insert(cache_key, (entry_sha256, Arc::clone(&index)));
    Ok(index)
}

fn read(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult<SparseIndex> {
    require_sparse_kind(entry, slot)?;
    let path = vault_dir.join(entry.require_index_rel(slot)?);
    if !path.is_file() {
        return Err(stale(format!(
            "persistent sparse sidecar missing at {}; rebuild the vault search indexes",
            path.display()
        )));
    }
    let bytes = fs::read(&path)?;
    let actual = sha256_hex(&bytes);
    let expected = entry.require_sha256(slot)?;
    if actual != expected {
        return Err(stale(format!(
            "persistent sparse sidecar sha256 {actual} != manifest {expected}; rebuild the vault search indexes"
        )));
    }
    let mut index: SparseIndex = serde_json::from_slice(&bytes).map_err(|err| {
        stale(format!(
            "persistent sparse sidecar {} is not valid JSON: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    // Derived from the payload, not read from it: `field_docs` is `#[serde(skip)]`
    // precisely so a persisted copy cannot disagree with the rows it counts.
    index.field_docs = field_doc_count(&index.rows);
    validate(&index, entry, manifest_base_seq, slot)?;
    Ok(index)
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult {
    if entry.require_index_rel(slot)?.ends_with(".sparse.jsonl") {
        visit_stream(vault_dir, entry, manifest_base_seq, slot, |_, _| Ok(()))?;
    } else {
        let _ = read(vault_dir, entry, manifest_base_seq, slot)?;
    }
    Ok(())
}

fn validate(
    index: &SparseIndex,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult {
    if index.format != SPARSE_FORMAT_V2 && index.format != SPARSE_FORMAT_V3 {
        return Err(stale(format!(
            "persistent sparse sidecar has format {}; expected {SPARSE_FORMAT_V2} or {SPARSE_FORMAT_V3}",
            index.format
        )));
    }
    let expected_kind = if index.format == SPARSE_FORMAT_V2 {
        if index.scoring != SparseScoring::Bm25 {
            return Err(stale(
                "persistent sparse v2 sidecar declares non-BM25 scoring; rebuild the vault search indexes",
            ));
        }
        LEGACY_BM25_KIND
    } else {
        index.scoring.index_kind()
    };
    entry.require_kind(expected_kind, slot)?;
    if index.slot != slot.get() || entry.slot != slot.get() {
        return Err(stale(format!(
            "persistent sparse sidecar slot {} / entry slot {} != query slot {}",
            index.slot,
            entry.slot,
            slot.get()
        )));
    }
    let entry_dim = entry.require_dim(slot)?;
    if index.dim != entry_dim {
        return Err(stale(format!(
            "persistent sparse sidecar dim {} != manifest dim {entry_dim}; rebuild the vault search indexes",
            index.dim
        )));
    }
    if index.base_seq != manifest_base_seq || entry.built_at_seq != manifest_base_seq {
        return Err(stale(format!(
            "persistent sparse sidecar seq {} / entry seq {} != manifest seq {manifest_base_seq}; rebuild the vault search indexes",
            index.base_seq, entry.built_at_seq
        )));
    }
    if index.rows.len() != entry.len {
        return Err(stale(format!(
            "persistent sparse sidecar row len {} != manifest len {}; rebuild the vault search indexes",
            index.rows.len(),
            entry.len
        )));
    }
    let mut seen = BTreeSet::new();
    for row in &index.rows {
        if !seen.insert(row.cx_id) {
            return Err(stale(format!(
                "persistent sparse sidecar repeats {}; rebuild the vault search indexes",
                row.cx_id
            )));
        }
        let expected_doc_len =
            validate_sparse_weights(&row.entries, index.scoring, &format!("row {}", row.cx_id))?;
        if row.doc_len.to_bits() != expected_doc_len.to_bits() {
            return Err(stale(format!(
                "persistent sparse row {} doc_len {} != weight sum {expected_doc_len}; rebuild the vault search indexes",
                row.cx_id, row.doc_len
            )));
        }
        SlotVector::Sparse {
            dim: index.dim,
            entries: row.entries.clone(),
        }
        .validate_schema()
        .map_err(|err| {
            stale(format!(
                "persistent sparse row {} has invalid payload: {}; rebuild the vault search indexes",
                row.cx_id, err.message
            ))
        })?;
    }
    let expected = postings_from_rows(&index.rows);
    if expected != index.postings {
        return Err(stale(
            "persistent sparse postings do not match row payloads; rebuild the vault search indexes",
        ));
    }
    let (expected_doc_lengths, expected_avg_doc_len) = sparse_stats(&index.rows)?;
    if index.doc_lengths != expected_doc_lengths {
        return Err(stale(
            "persistent sparse doc-length metadata does not match row payloads; rebuild the vault search indexes",
        ));
    }
    if (index.avg_doc_len - expected_avg_doc_len).abs() > f32::EPSILON {
        return Err(stale(
            "persistent sparse average doc length does not match row payloads; rebuild the vault search indexes",
        ));
    }
    Ok(())
}

fn postings_from_rows(rows: &[SparseRow]) -> BTreeMap<u32, Vec<SparsePosting>> {
    let mut out = BTreeMap::<u32, Vec<SparsePosting>>::new();
    for row in rows {
        for entry in &row.entries {
            out.entry(entry.idx).or_default().push(SparsePosting {
                cx_id: row.cx_id,
                tf: entry.val,
            });
        }
    }
    out
}

/// Rows carrying at least one term in this lane — BM25's `N`.
///
/// A row whose vector is empty is a document that does not have this field at
/// all. It is retained in `rows` (the lane must know the row exists so a delta
/// can mask it) but it is not a document the field's statistics describe.
fn field_doc_count(rows: &[SparseRow]) -> usize {
    rows.iter().filter(|row| !row.entries.is_empty()).count()
}

fn sparse_stats(rows: &[SparseRow]) -> CliResult<(BTreeMap<CxId, f32>, f32)> {
    let doc_lengths = rows
        .iter()
        .map(|row| (row.cx_id, row.doc_len))
        .collect::<BTreeMap<_, _>>();
    let mut total_doc_len = 0.0_f32;
    for doc_len in doc_lengths.values() {
        total_doc_len += doc_len;
        if !total_doc_len.is_finite() {
            return Err(stale("persistent sparse corpus length overflowed"));
        }
    }
    // Averaged over the documents that carry the field, never over every row in
    // the panel: see `SparseIndex::field_docs`.
    let field_docs = field_doc_count(rows);
    let avg_doc_len = if field_docs == 0 {
        0.0
    } else {
        total_doc_len / field_docs as f32
    };
    Ok((doc_lengths, avg_doc_len))
}

fn score(
    index: &SparseIndex,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<(CxId, f32)>> {
    if index.scoring == SparseScoring::DotProduct {
        return score_dot_product(index, query, candidates);
    }
    // BM25's N is the number of documents that carry this field, matching the
    // `avg_doc_len` denominator. Using every panel row would deflate IDF for
    // every term on a panel where most rows have no text at all.
    let total_docs = index.field_docs;
    let scorer = Bm25::default();
    let mut scores = BTreeMap::<CxId, f32>::new();
    for query_entry in query {
        let Some(postings) = index.postings.get(&query_entry.idx) else {
            continue;
        };
        let df = postings.len();
        for posting in postings {
            if candidates.is_some_and(|allowed| !allowed.contains(&posting.cx_id)) {
                continue;
            }
            let len = *index.doc_lengths.get(&posting.cx_id).unwrap_or(&1.0);
            let contribution =
                scorer.score_term(posting.tf, len, index.avg_doc_len, total_docs, df)
                    * query_entry.val;
            let score = scores.entry(posting.cx_id).or_default();
            *score += contribution;
            if !score.is_finite() {
                return Err(stale(format!(
                    "persistent sparse score overflowed for {}; rebuild the vault search indexes",
                    posting.cx_id
                )));
            }
        }
    }
    Ok(scores.into_iter().collect())
}

fn score_dot_product(
    index: &SparseIndex,
    query: &[SparseEntry],
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<(CxId, f32)>> {
    let mut scores = BTreeMap::<CxId, f32>::new();
    for query_entry in query {
        let Some(postings) = index.postings.get(&query_entry.idx) else {
            continue;
        };
        for posting in postings {
            if candidates.is_some_and(|allowed| !allowed.contains(&posting.cx_id)) {
                continue;
            }
            let contribution = posting.tf * query_entry.val;
            let score = scores.entry(posting.cx_id).or_default();
            *score += contribution;
            if !score.is_finite() {
                return Err(stale(format!(
                    "persistent sparse dot-product score overflowed for {}; rebuild the vault search indexes",
                    posting.cx_id
                )));
            }
        }
    }
    Ok(scores.into_iter().collect())
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
        LEGACY_BM25_KIND | "sparse_bm25" | "sparse_dot" => Ok(()),
        other => Err(stale(format!(
            "persistent slot {slot} index kind {other} is not a supported sparse index; rebuild the vault search indexes"
        ))),
    }
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
