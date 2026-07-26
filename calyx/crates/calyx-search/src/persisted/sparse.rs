use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use calyx_core::{CxId, SlotId, SlotVector, SparseEntry};
use calyx_sextant::index::bm25::Bm25;
use calyx_sextant::index::{IndexSearchHit, ranked};
use serde::{Deserialize, Serialize};

use super::pinned::{self, PinKey};
use super::{SearchIndexEntry, rel, sha256_hex, stale, write_json_atomic_hashed};
use crate::error::CliResult;

const SPARSE_FORMAT_V2: &str = "calyx-search-sparse-index-v2";
const SPARSE_FORMAT_V3: &str = "calyx-search-sparse-index-v3";
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

pub(super) fn write(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    rows: SparseSlotRows,
    base_seq: u64,
    scoring: SparseScoring,
) -> CliResult<SearchIndexEntry> {
    let path = root.join(format!(
        "slot_{:05}_seq_{base_seq:020}_n_{:010}.sparse.json",
        slot.get(),
        rows.rows.len()
    ));
    let index = build_index(slot, rows.dim, rows.rows, base_seq, scoring)?;
    let sha256 = write_json_atomic_hashed(&path, &index)?;
    Ok(SearchIndexEntry::sparse(
        slot,
        index.dim,
        index.rows.len(),
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
    let index = pinned_index(vault_dir, entry, manifest_base_seq, slot)?;
    validate_sparse_weights(entries, index.scoring, "query")?;
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

fn build_index(
    slot: SlotId,
    dim: u32,
    source_rows: Vec<(CxId, Vec<SparseEntry>)>,
    base_seq: u64,
    scoring: SparseScoring,
) -> CliResult<SparseIndex> {
    let rows = source_rows
        .into_iter()
        .map(|(cx_id, entries)| {
            let doc_len = validate_sparse_weights(&entries, scoring, &format!("row {cx_id}"))?;
            Ok(SparseRow {
                cx_id,
                doc_len,
                entries,
            })
        })
        .collect::<CliResult<Vec<_>>>()?;
    let postings = postings_from_rows(&rows);
    let (doc_lengths, avg_doc_len) = sparse_stats(&rows)?;
    Ok(SparseIndex {
        format: SPARSE_FORMAT_V3.to_string(),
        scoring,
        slot: slot.get(),
        dim,
        base_seq,
        rows,
        postings,
        doc_lengths,
        avg_doc_len,
    })
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
    let index: SparseIndex = serde_json::from_slice(&bytes).map_err(|err| {
        stale(format!(
            "persistent sparse sidecar {} is not valid JSON: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    validate(&index, entry, manifest_base_seq, slot)?;
    Ok(index)
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult {
    let _ = read(vault_dir, entry, manifest_base_seq, slot)?;
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
    let avg_doc_len = if rows.is_empty() {
        0.0
    } else {
        total_doc_len / rows.len() as f32
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
    let total_docs = index.rows.len();
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

fn validate_sparse_weights(
    entries: &[SparseEntry],
    scoring: SparseScoring,
    context: &str,
) -> CliResult<f32> {
    let mut total = 0.0_f32;
    for entry in entries {
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
