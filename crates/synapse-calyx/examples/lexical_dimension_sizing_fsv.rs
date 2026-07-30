//! Manual FSV instrument for #1904 ask 4: choose a sparse dimension from the
//! corpus's actual token distribution rather than copying the timeline's 2048.
//!
//! ## Why the timeline's dimension cannot just be reused
//!
//! `syn_sparse_text_tf` is a hashing-trick encoder: a term is hashed into one of
//! `dim` cells. Two distinct terms landing in one cell are indistinguishable —
//! the lane scores a query for term A against documents that only ever contained
//! term B. The rate of that depends on the corpus **vocabulary** (how many
//! distinct terms exist) against `dim`, not on how long any one document is.
//!
//! The timeline panel's lane hashes window titles: a small, highly repetitive
//! vocabulary of app names and file names. An agent transcript is prose. Copying
//! 2048 would be sizing the second corpus by the first corpus's statistics.
//!
//! ## What this measures, on real data
//!
//! The authoritative source of the agent-transcript corpus is the provider's own
//! session JSONL (`source: claude_session_jsonl` on the stored rows). This reads
//! those files directly, reconstructs the text the transcript panel actually
//! measures — assistant/thinking text, truncated to
//! `AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS` exactly as `set_content` does — and runs
//! the **production** `syn_sparse_text_tf` encoder over it at several candidate
//! dimensions.
//!
//! For each candidate it reports the load factor and the measured **collision
//! rate**: the fraction of distinct terms that had to share a cell with another
//! distinct term. That is the number the dimension has to be chosen against, and
//! it is measured rather than assumed.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example lexical_dimension_sizing_fsv -- <sessions-root> [max-files]`

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use calyx_core::{Input, Lens as _, Modality, SlotVector};
use calyx_registry::AlgorithmicLens;

/// Mirrors `AGENT_TRANSCRIPT_MAX_SUMMARY_CHARS`; a stored `content_summary` is
/// truncated to this many characters, so the measured text never exceeds it.
const MAX_SUMMARY_CHARS: usize = 2048;
const CANDIDATE_DIMS: &[u32] = &[2048, 4096, 8192, 16384, 32768, 65536];

fn session_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out.truncate(limit);
    out
}

/// Reconstructs the text the transcript panel measures from one session line.
///
/// `transcript_text` joins `content_summary` (plus any error strings); the
/// summary is the assistant/thinking text bounded by `set_content`. Lines that
/// carry no such content produce no text — exactly as they store no summary.
fn line_text(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let content = value.get("message")?.get("content")?;
    let mut parts = Vec::new();
    match content {
        serde_json::Value::String(text) => parts.push(text.clone()),
        serde_json::Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                            parts.push(text.to_owned());
                        }
                    }
                    Some("thinking") => {
                        if let Some(text) =
                            block.get("thinking").and_then(serde_json::Value::as_str)
                        {
                            parts.push(text.to_owned());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("\n");
    // The stored summary is character-bounded, so the measured text is too.
    Some(joined.chars().take(MAX_SUMMARY_CHARS).collect())
}

/// Production tokenization, reached the only honest way: by asking a very wide
/// encoder to measure the term and reading which single cell it produced. A
/// one-term document yields a one-cell vector, so the cell IS the term's hash at
/// that dimension, and no tokenizer is reimplemented here.
fn cells(lens: &AlgorithmicLens, text: &str) -> Result<Vec<u32>, Box<dyn Error>> {
    let vector = lens.measure(&Input::new(Modality::Text, text.as_bytes().to_vec()))?;
    let SlotVector::Sparse { entries, .. } = vector else {
        return Err("expected a sparse vector".into());
    };
    Ok(entries.into_iter().map(|entry| entry.idx).collect())
}

/// `usize -> u64` without an unchecked cast (lossless on every supported target).
fn count_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// `u64 / u64` as `f64` without an unchecked `as` cast on either side.
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    let scaled = (numerator.saturating_mul(1_000_000)) / denominator;
    u32::try_from(scaled).map_or(f64::NAN, |scaled| f64::from(scaled) / 1_000_000.0)
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .ok_or("usage: lexical_dimension_sizing_fsv <sessions-root> [max-files]")?,
    );
    let limit = args
        .next()
        .map_or(Ok(usize::MAX), |value| value.parse::<usize>())?;

    let files = session_files(&root, limit);
    println!(
        "lexical_dimension_sizing_fsv: {} session files",
        files.len()
    );

    // Split each document into its terms using the production tokenizer, by
    // measuring the whole document at a very wide dimension and separately
    // measuring each whitespace-delimited candidate term. The vocabulary is
    // collected as the set of terms the encoder actually accepted.
    let probe = AlgorithmicLens::syn_sparse_text_tf("fsv.probe.v1", Modality::Text, 1 << 20);

    let mut vocabulary: BTreeSet<String> = BTreeSet::new();
    let mut documents = 0_usize;
    let mut total_tokens = 0_u64;
    let mut doc_token_counts: Vec<usize> = Vec::new();

    for file in &files {
        let Ok(content) = fs::read_to_string(file) else {
            continue;
        };
        for line in content.lines() {
            let Some(text) = line_text(line) else {
                continue;
            };
            // Candidate terms, split the way the production tokenizer splits.
            let terms = text
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '-'))
                .filter(|token| !token.is_empty())
                .filter(|token| token.chars().any(char::is_alphanumeric))
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>();
            if terms.is_empty() {
                continue;
            }
            documents += 1;
            total_tokens = total_tokens.saturating_add(count_u64(terms.len()));
            doc_token_counts.push(terms.len());
            for term in terms {
                vocabulary.insert(term);
            }
        }
    }

    if documents == 0 {
        return Err("no text-bearing lines found; check the sessions root".into());
    }
    doc_token_counts.sort_unstable();
    // Integer percentile index: no float round-trip, so no truncation lint and
    // no ambiguity about which element a percentile names.
    let percentile = |numerator: usize, denominator: usize| -> usize {
        let last = doc_token_counts.len().saturating_sub(1);
        doc_token_counts[last * numerator / denominator]
    };
    println!("\n=== measured corpus ===");
    println!("text_bearing_documents = {documents}");
    println!("total_tokens           = {total_tokens}");
    println!("distinct_terms         = {}", vocabulary.len());
    println!(
        "tokens_per_document    min={} p50={} p90={} p99={} max={} mean={:.1}",
        doc_token_counts[0],
        percentile(50, 100),
        percentile(90, 100),
        percentile(99, 100),
        doc_token_counts[doc_token_counts.len() - 1],
        ratio(total_tokens, count_u64(documents))
    );

    // Confirm the production encoder agrees that one term is one cell, so the
    // per-dimension collision counts below are about hashing and not about
    // tokenization disagreeing with this driver.
    let sample = vocabulary
        .iter()
        .next()
        .ok_or("vocabulary is empty after scanning a non-empty corpus")?;
    let sample_cells = cells(&probe, sample)?;
    println!(
        "tokenizer_agreement: term {sample:?} -> {} cell(s) at dim 2^20",
        sample_cells.len()
    );

    println!("\n=== collision rate by candidate dimension ===");
    println!("  dim      load_factor  distinct_cells  colliding_terms  collision_rate");
    for &dim in CANDIDATE_DIMS {
        let lens = AlgorithmicLens::syn_sparse_text_tf("fsv.candidate.v1", Modality::Text, dim);
        let mut per_cell: BTreeMap<u32, usize> = BTreeMap::new();
        for term in &vocabulary {
            let term_cells = cells(&lens, term)?;
            if let Some(cell) = term_cells.first() {
                *per_cell.entry(*cell).or_default() += 1;
            }
        }
        let colliding: usize = per_cell.values().filter(|n| **n > 1).sum();
        let rate = ratio(count_u64(colliding), count_u64(vocabulary.len()));
        println!(
            "  {dim:<8} {:<12.4} {:<15} {colliding:<16} {:.4}",
            ratio(count_u64(vocabulary.len()), u64::from(dim)),
            per_cell.len(),
            rate
        );
    }

    Ok(())
}
