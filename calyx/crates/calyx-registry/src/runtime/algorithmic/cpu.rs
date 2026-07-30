use std::collections::BTreeMap;

use calyx_core::{Result, SlotVector, SparseEntry, content_address};

use super::BYTE_FEATURE_DIM;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub(super) fn byte_features(bytes: &[u8]) -> Vec<f32> {
    if bytes.is_empty() {
        let mut out = vec![0.0_f32; BYTE_FEATURE_DIM as usize];
        out[0] = 1.0;
        return out;
    }
    let mut raw = [0_u64; 15];
    raw[0] = bytes.len().min(u32::MAX as usize) as u64;
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        raw[13] += u64::from(byte);
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
        raw[1] += byte.is_ascii() as u64;
        raw[2] += byte.is_ascii_whitespace() as u64;
        raw[3] += byte.is_ascii_alphabetic() as u64;
        raw[4] += byte.is_ascii_digit() as u64;
        raw[5] += byte.is_ascii_punctuation() as u64;
        raw[6] += byte.is_ascii_uppercase() as u64;
        raw[7] += byte.is_ascii_lowercase() as u64;
        raw[8] += byte.is_ascii_control() as u64;
        raw[9] += (byte == 0) as u64;
        raw[10] += matches!(byte, b'/' | b'\\') as u64;
        raw[11] += matches!(byte, b'{' | b'}' | b'(' | b')' | b'[' | b']') as u64;
        raw[12] += matches!(byte, b'\n' | b'\r') as u64;
    }
    raw[14] = hash;
    byte_features_from_raw(raw)
}

pub(super) fn byte_features_from_raw(raw: [u64; 15]) -> Vec<f32> {
    let mut out = vec![0.0_f32; BYTE_FEATURE_DIM as usize];
    if raw[0] == 0 {
        out[0] = 1.0;
        return out;
    }
    let len = (raw[0].min(u64::from(u32::MAX)) as u32) as f32;
    let inv_len = 1.0 / len.max(1.0);
    out[0] = len.log2().max(0.0) / 32.0;
    for index in 1..=12 {
        out[index] = (raw[index] as u32) as f32 * inv_len;
    }
    out[13] = raw[13] as f32 / (len * 255.0);
    out[14] = hash_part((raw[14] & 0xffff_ffff) as u32);
    out[15] = hash_part((raw[14] >> 32) as u32);
    out
}

pub(super) fn hash_part(value: u32) -> f32 {
    (value as f32 / u32::MAX as f32) * 2.0 - 1.0
}

pub(super) fn scalar_features(bytes: &[u8]) -> Vec<f32> {
    if bytes.is_empty() {
        return vec![0.0];
    }
    let mean = bytes.iter().map(|byte| f32::from(*byte)).sum::<f32>() / bytes.len() as f32;
    vec![(mean - 80.0) / 80.0]
}

pub(super) fn one_hot_features(bytes: &[u8], buckets: u32) -> Vec<f32> {
    let buckets = buckets.max(1);
    let mut out = vec![0.0; buckets as usize];
    let hash = bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    out[(hash % u64::from(buckets)) as usize] = 1.0;
    out
}

pub(super) fn ast_style_features(bytes: &[u8]) -> Vec<f32> {
    let text = String::from_utf8_lossy(bytes);
    let len = bytes.len().max(1) as f32;
    let count = |needle: &str| text.matches(needle).count() as f32 / len;
    vec![
        count("fn"),
        count("let"),
        count("struct"),
        count("impl"),
        bytes.iter().filter(|b| matches!(b, b'{' | b'}')).count() as f32 / len,
        bytes.iter().filter(|b| **b == b';').count() as f32 / len,
        bytes.iter().filter(|b| **b == b'(').count() as f32 / len,
        bytes.iter().filter(|b| **b == b'\n').count() as f32 / len,
    ]
}

pub(super) fn sparse_keywords(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    sparse_keywords_from_hashes(&keyword_hashes(bytes), dim)
}

/// The whitespace-term hashing shared by the normalized and raw-count lanes, so
/// the two encoders differ only in normalization and never in bucketing.
fn keyword_hashes(bytes: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .map(|term| {
            let digest = content_address([term.as_bytes()]);
            u32::from_be_bytes(digest[..4].try_into().expect("content hash has bytes"))
        })
        .collect()
}

pub(super) fn sparse_keywords_from_hashes(hashes: &[u32], dim: u32) -> Result<SlotVector> {
    let dim = dim.max(1);
    Ok(SlotVector::Sparse {
        dim,
        entries: normalized_entries(&keyword_counts(hashes, dim)),
    })
}

/// Raw hashed term frequencies over whitespace terms: unsigned, unnormalized.
///
/// The BM25-scorable sibling of `sparse_keywords`, which L1-normalizes. That
/// normalization is correct for a similarity lane and fatal for a ranking one:
/// it makes every stored vector sum to exactly 1.0, so the document length
/// BM25 derives from the weight sum is 1.0 for *every* row in the corpus, and
/// `b`/`avgdl` are computed, persisted and validated while being unable to move
/// a single score (#1902). `tf` also stops being a count and becomes a relative
/// frequency, so a one-token document scores 1.0 on its only term while a
/// hundred-token document scores 0.01 on each of its -- the exact
/// short-document dominance `b` exists to bound.
///
/// The hashing is byte-identical to `sparse_keywords`; only the normalization
/// is dropped. Both encoders stay data-oblivious, and the corpus statistics
/// stay in the index where they belong.
pub(super) fn sparse_keywords_tf(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    sparse_keywords_tf_from_hashes(&keyword_hashes(bytes), dim)
}

pub(super) fn sparse_keywords_tf_from_hashes(hashes: &[u32], dim: u32) -> Result<SlotVector> {
    let dim = dim.max(1);
    Ok(SlotVector::Sparse {
        dim,
        entries: keyword_counts(hashes, dim)
            .into_iter()
            .map(|(idx, val)| SparseEntry { idx, val })
            .collect(),
    })
}

fn keyword_counts(hashes: &[u32], dim: u32) -> BTreeMap<u32, f32> {
    let mut counts = BTreeMap::<u32, f32>::new();
    for &hash in hashes {
        *counts.entry(hash % dim).or_default() += 1.0;
    }
    counts
}

fn normalized_entries(counts: &BTreeMap<u32, f32>) -> Vec<SparseEntry> {
    let total = counts.values().sum::<f32>().max(1.0);
    counts
        .iter()
        .map(|(idx, val)| SparseEntry {
            idx: *idx,
            val: val / total,
        })
        .collect()
}

pub(super) fn token_hash(bytes: &[u8], token_dim: u32) -> Result<SlotVector> {
    let token_dim = token_dim.max(1);
    let mut tokens = tokenize_token_hash(bytes)
        .iter()
        .map(|term| token_vector(term, token_dim))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        tokens.push(token_vector(bytes, token_dim));
    }
    Ok(SlotVector::Multi { token_dim, tokens })
}

pub(super) fn tokenize_sparse(bytes: &[u8]) -> Vec<Vec<u8>> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .map(str::as_bytes)
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn tokenize_token_hash(bytes: &[u8]) -> Vec<Vec<u8>> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .take(32)
        .map(str::as_bytes)
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(feature = "cuda")]
pub(super) fn token_vectors_from_words(words: &[u32], token_dim: u32) -> Vec<Vec<f32>> {
    words
        .chunks_exact(token_dim.max(1) as usize)
        .map(|row| row.iter().copied().map(hash_part).collect())
        .collect()
}

pub(super) fn token_vector(seed: &[u8], dim: u32) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim as usize);
    let mut counter = 0_u32;
    while out.len() < dim as usize {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"calyx-algorithmic-token-hash-v1");
        hasher.update(seed);
        hasher.update(&counter.to_be_bytes());
        for chunk in hasher.finalize().as_bytes().chunks_exact(4) {
            let raw = u32::from_be_bytes(chunk.try_into().expect("blake3 chunk is 4 bytes"));
            out.push(hash_part(raw));
            if out.len() == dim as usize {
                break;
            }
        }
        counter = counter.saturating_add(1);
    }
    out
}
