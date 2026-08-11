//! Verified, pinned in-memory multi-vector (MaxSim) index. Each manifest
//! generation is loaded once: whole-file reads, one-shot sha256 against the
//! manifest, full structural validation (header, duplicate rows, token
//! counts, finiteness, trailing bytes), then a flattened token matrix with
//! precomputed token norms is pinned. Queries score with the exact dispatched
//! dot/norm arithmetic of `MaxSimIndex::maxsim`, so results are bit-identical
//! between the live and persisted paths on the same host.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use calyx_sextant::index::distance::dot;
use rayon::prelude::*;

use super::*;
use crate::persisted::pinned::{self, PinKey};

const PIN_KIND: &str = "multi_maxsim";

#[path = "pinned/bounded.rs"]
mod bounded;
pub(super) use bounded::{
    BoundedSegmentFile, memoize_bounded_segment_files, memoized_bounded_segment_files,
    stat_check_segment_files,
};

/// One verified binary segment to load: path plus the manifest-side
/// expectations the file must match.
pub(super) struct PinnedSegmentSpec {
    pub(super) path: PathBuf,
    pub(super) index_rel: String,
    pub(super) sha256: String,
    pub(super) base_seq: u64,
    pub(super) row_count: u64,
    pub(super) token_count: u64,
}

#[derive(Debug)]
pub(super) struct PinnedMultiIndex {
    token_dim: u32,
    rows: Vec<PinnedMultiRow>,
    tokens: Vec<f32>,
    norms: Vec<f32>,
}

#[derive(Debug)]
struct PinnedMultiRow {
    cx_id: CxId,
    token_start: usize,
    token_count: usize,
}

struct PinnedGeneration {
    entry_sha256: String,
    index: Arc<PinnedMultiIndex>,
}

type PinCache = Mutex<BTreeMap<(String, u16), PinnedGeneration>>;

fn cache() -> &'static PinCache {
    static CACHE: OnceLock<PinCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Return the pinned index for `entry`, loading and verifying it on first
/// use per manifest generation. The cache key is the manifest entry sha256:
/// any rebuild produces a new sha and forces a fresh verified load.
pub(super) fn pinned_index(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
    specs: Vec<PinnedSegmentSpec>,
) -> CliResult<Arc<PinnedMultiIndex>> {
    let entry_sha256 = entry.require_sha256(slot)?.to_string();
    let cache_key = (pinned::canonical_vault_dir(vault_dir)?, slot.get());
    {
        let cache = cache().lock().expect("multi pin cache poisoned");
        if let Some(generation) = cache.get(&cache_key)
            && generation.entry_sha256 == entry_sha256
        {
            return Ok(Arc::clone(&generation.index));
        }
    }
    let token_dim = entry.require_token_dim(slot)?;
    let pin_key = PinKey::new(vault_dir, slot.get(), PIN_KIND)?;
    let predicted_bytes = predicted_pin_bytes(entry, token_dim)?;
    pinned::reserve(&pin_key, predicted_bytes)?;
    let index = match load_verified(slot, token_dim, entry, &specs, &pin_key, predicted_bytes) {
        Ok(index) => Arc::new(index),
        Err(error) => {
            pinned::release(&pin_key);
            // The failed generation vouches for nothing; drop any stale
            // cached generation for this key so its memory is not held
            // without a matching budget reservation.
            cache()
                .lock()
                .expect("multi pin cache poisoned")
                .remove(&cache_key);
            return Err(error);
        }
    };
    // Replace the prediction with the exact footprint. On failure (another
    // thread reserved in between) the prediction must be released too, or the
    // key would hold phantom budget bytes with nothing pinned.
    if let Err(error) = pinned::reserve(&pin_key, index.approx_bytes()) {
        pinned::release(&pin_key);
        return Err(error);
    }
    let mut cache = cache().lock().expect("multi pin cache poisoned");
    cache.insert(
        cache_key,
        PinnedGeneration {
            entry_sha256,
            index: Arc::clone(&index),
        },
    );
    Ok(index)
}

fn predicted_pin_bytes(entry: &SearchIndexEntry, token_dim: u32) -> CliResult<u64> {
    let tokens = entry.token_count.unwrap_or_default() as u64;
    tokens
        .checked_mul(token_dim as u64)
        .and_then(|values| values.checked_mul(4))
        .and_then(|bytes| bytes.checked_add(tokens.checked_mul(4)?))
        .and_then(|bytes| {
            bytes.checked_add(
                (entry.len as u64).checked_mul(std::mem::size_of::<PinnedMultiRow>() as u64)?,
            )
        })
        .ok_or_else(|| stale("persistent segmented multi sidecar pin byte count overflow"))
}

fn load_verified(
    slot: SlotId,
    token_dim: u32,
    entry: &SearchIndexEntry,
    specs: &[PinnedSegmentSpec],
    pin_key: &PinKey,
    predicted_bytes: u64,
) -> CliResult<PinnedMultiIndex> {
    let segments = specs
        .par_iter()
        .map(|spec| load_segment(slot, token_dim, spec))
        .collect::<Vec<_>>();
    let expected_tokens = entry.token_count.unwrap_or_default();
    let expected_values = expected_tokens
        .checked_mul(token_dim as usize)
        .ok_or_else(|| stale("persistent segmented multi sidecar token byte count overflow"))?;
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    rows.try_reserve_exact(entry.len)
        .map_err(|_| pinned::pin_allocation_error(pin_key, predicted_bytes))?;
    let mut tokens: Vec<f32> = Vec::new();
    tokens
        .try_reserve_exact(expected_values)
        .map_err(|_| pinned::pin_allocation_error(pin_key, predicted_bytes))?;
    let mut norms: Vec<f32> = Vec::new();
    norms
        .try_reserve_exact(expected_tokens)
        .map_err(|_| pinned::pin_allocation_error(pin_key, predicted_bytes))?;
    let mut token_offset = 0usize;
    for segment in segments {
        let segment = segment?;
        for (cx_id, token_count) in segment.rows {
            if !seen.insert(cx_id) {
                return Err(stale(format!(
                    "persistent segmented multi sidecars repeat {cx_id}; rebuild the vault search indexes"
                )));
            }
            rows.push(PinnedMultiRow {
                cx_id,
                token_start: token_offset,
                token_count,
            });
            token_offset = token_offset
                .checked_add(token_count)
                .ok_or_else(|| stale("persistent segmented multi sidecar token_count overflow"))?;
        }
        tokens.extend_from_slice(&segment.tokens);
        norms.extend_from_slice(&segment.norms);
        if token_offset != norms.len() || tokens.len() != norms.len() * token_dim as usize {
            return Err(stale(format!(
                "persistent segmented multi sidecar {} token layout is inconsistent; rebuild the vault search indexes",
                entry.require_index_rel(slot)?
            )));
        }
    }
    if rows.len() != entry.len {
        return Err(stale(format!(
            "persistent segmented multi manifest row_count {} != scanned row count {}; rebuild the vault search indexes",
            entry.len,
            rows.len()
        )));
    }
    if norms.len() != expected_tokens {
        return Err(stale(format!(
            "persistent segmented multi manifest token_count {expected_tokens} != scanned token count {}; rebuild the vault search indexes",
            norms.len()
        )));
    }
    Ok(PinnedMultiIndex {
        token_dim,
        rows,
        tokens,
        norms,
    })
}

struct LoadedSegment {
    rows: Vec<(CxId, usize)>,
    tokens: Vec<f32>,
    norms: Vec<f32>,
}

fn load_segment(
    slot: SlotId,
    token_dim: u32,
    spec: &PinnedSegmentSpec,
) -> CliResult<LoadedSegment> {
    let bytes = fs::read(&spec.path)?;
    if bytes.len() < 16 || &bytes[..16] != super::binary::MULTI_BINARY_MAGIC {
        return Err(stale(format!(
            "persistent binary multi sidecar {} has invalid magic; rebuild the vault search indexes",
            spec.index_rel
        )));
    }
    let actual = sha256_hex(&bytes);
    if actual != spec.sha256 {
        return Err(stale(format!(
            "persistent binary multi sidecar sha256 {actual} != manifest {}; rebuild the vault search indexes",
            spec.sha256
        )));
    }
    parse_segment(slot, token_dim, spec, &bytes)
}

fn parse_segment(
    slot: SlotId,
    token_dim: u32,
    spec: &PinnedSegmentSpec,
    bytes: &[u8],
) -> CliResult<LoadedSegment> {
    let mut cursor = SegmentCursor::new(&spec.index_rel, bytes);
    cursor.expect_magic()?;
    let header_slot = cursor.read_u16()?;
    let header_token_dim = cursor.read_u32()?;
    let header_base_seq = cursor.read_u64()?;
    let header_row_count = cursor.read_u64()?;
    let header_token_count = cursor.read_u64()?;
    if header_slot != slot.get() {
        return Err(stale(format!(
            "persistent binary multi sidecar slot {header_slot} != expected slot {}; rebuild the vault search indexes",
            slot.get()
        )));
    }
    if header_token_dim != token_dim {
        return Err(stale(format!(
            "persistent binary multi sidecar token_dim {header_token_dim} != expected token_dim {token_dim}; rebuild the vault search indexes"
        )));
    }
    if header_base_seq != spec.base_seq {
        return Err(stale(format!(
            "persistent segmented multi sidecar {} seq {header_base_seq} != segment manifest seq {}; rebuild the vault search indexes",
            spec.index_rel, spec.base_seq
        )));
    }
    if header_row_count != spec.row_count {
        return Err(stale(format!(
            "persistent binary multi sidecar row len {header_row_count} != expected {}; rebuild the vault search indexes",
            spec.row_count
        )));
    }
    if header_token_count != spec.token_count {
        return Err(stale(format!(
            "persistent binary multi sidecar token_count {header_token_count} != expected {}; rebuild the vault search indexes",
            spec.token_count
        )));
    }
    let dim = token_dim as usize;
    let segment_values = (header_token_count as usize)
        .checked_mul(dim)
        .ok_or_else(|| stale("persistent binary multi sidecar token byte count overflow"))?;
    let mut rows = Vec::with_capacity(header_row_count as usize);
    let mut seen = BTreeSet::new();
    let mut tokens: Vec<f32> = Vec::with_capacity(segment_values);
    let mut norms: Vec<f32> = Vec::with_capacity(header_token_count as usize);
    let mut observed_tokens = 0u64;
    for _ in 0..header_row_count {
        let cx_id = cursor.read_cx_id()?;
        if !seen.insert(cx_id) {
            return Err(stale(format!(
                "persistent binary multi segmented sidecar repeats {cx_id}; rebuild the vault search indexes"
            )));
        }
        let row_token_count = cursor.read_u32()? as u64;
        observed_tokens = observed_tokens
            .checked_add(row_token_count)
            .ok_or_else(|| stale("persistent binary multi sidecar token_count overflow"))?;
        if observed_tokens > header_token_count {
            return Err(stale(format!(
                "persistent binary multi sidecar token_count exceeds header {header_token_count}; rebuild the vault search indexes"
            )));
        }
        let row_tokens = row_token_count as usize;
        let payload_bytes = cursor.take(
            row_tokens
                .checked_mul(dim)
                .and_then(|values| values.checked_mul(4))
                .ok_or_else(|| {
                    stale("persistent binary multi sidecar token byte count overflow")
                })?,
        )?;
        // Decode and finite-check directly into the flat matrix without an
        // intermediate per-row allocation. Norms use the same dispatched dot
        // kernel as `MaxSimIndex::maxsim`, which is the cross-path arithmetic
        // contract for live and persisted MaxSim scoring.
        for token_bytes in payload_bytes.chunks_exact(dim * 4) {
            let token_start = tokens.len();
            for value_bytes in token_bytes.chunks_exact(4) {
                let value = f32::from_le_bytes(value_bytes.try_into().expect("len 4"));
                if !value.is_finite() {
                    return Err(CalyxError::lens_numerical_invariant(format!(
                        "persistent binary multi row {cx_id} slot {slot} has non-finite component"
                    ))
                    .into());
                }
                tokens.push(value);
            }
            let token = &tokens[token_start..token_start + dim];
            norms.push(dot(token, token).sqrt());
        }
        rows.push((cx_id, row_tokens));
    }
    if observed_tokens != header_token_count {
        return Err(stale(format!(
            "persistent binary multi sidecar token_count {observed_tokens} != header {header_token_count}; rebuild the vault search indexes"
        )));
    }
    cursor.expect_end()?;
    Ok(LoadedSegment {
        rows,
        tokens,
        norms,
    })
}

impl PinnedMultiIndex {
    fn approx_bytes(&self) -> u64 {
        (self.tokens.len() as u64 + self.norms.len() as u64) * 4
            + self.rows.len() as u64 * std::mem::size_of::<PinnedMultiRow>() as u64
    }

    pub(super) fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// MaxSim over the pinned matrix; arithmetic mirrors
    /// `MaxSimIndex::maxsim` exactly (same dispatched dot/norm reduction and
    /// zero-norm semantics), parallelized across rows.
    pub(super) fn score(
        &self,
        query: &[Vec<f32>],
        candidates: Option<&BTreeSet<CxId>>,
    ) -> Vec<(CxId, f32)> {
        let dim = self.token_dim as usize;
        let query_norms = query
            .iter()
            .map(|token| dot(token, token).sqrt())
            .collect::<Vec<_>>();
        self.rows
            .par_iter()
            .filter(|row| candidates.is_none_or(|allowed| allowed.contains(&row.cx_id)))
            .map(|row| {
                let tokens =
                    &self.tokens[row.token_start * dim..(row.token_start + row.token_count) * dim];
                let norms = &self.norms[row.token_start..row.token_start + row.token_count];
                let score = query
                    .iter()
                    .zip(&query_norms)
                    .map(|(q, q_norm)| {
                        let mut best = f32::NEG_INFINITY;
                        for (token, t_norm) in tokens.chunks_exact(dim).zip(norms) {
                            let dot = dot(q, token);
                            let cosine = if *q_norm == 0.0 || *t_norm == 0.0 {
                                0.0
                            } else {
                                dot / (q_norm * t_norm)
                            };
                            best = best.max(cosine);
                        }
                        best
                    })
                    .sum::<f32>();
                (row.cx_id, score)
            })
            .collect()
    }
}

struct SegmentCursor<'a> {
    index_rel: &'a str,
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SegmentCursor<'a> {
    fn new(index_rel: &'a str, bytes: &'a [u8]) -> Self {
        Self {
            index_rel,
            bytes,
            offset: 0,
        }
    }

    fn take(&mut self, len: usize) -> CliResult<&'a [u8]> {
        let end = self.offset.checked_add(len).ok_or_else(|| {
            stale(format!(
                "persistent binary multi sidecar {} offset overflow; rebuild the vault search indexes",
                self.index_rel
            ))
        })?;
        if end > self.bytes.len() {
            return Err(stale(format!(
                "persistent binary multi sidecar {} is truncated; rebuild the vault search indexes",
                self.index_rel
            )));
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn expect_magic(&mut self) -> CliResult {
        if self.take(16)? != super::binary::MULTI_BINARY_MAGIC {
            return Err(stale(format!(
                "persistent binary multi sidecar {} has invalid magic; rebuild the vault search indexes",
                self.index_rel
            )));
        }
        Ok(())
    }

    fn read_u16(&mut self) -> CliResult<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("len 2")))
    }

    fn read_u32(&mut self) -> CliResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("len 4")))
    }

    fn read_u64(&mut self) -> CliResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("len 8")))
    }

    fn read_cx_id(&mut self) -> CliResult<CxId> {
        Ok(CxId::from_bytes(self.take(16)?.try_into().expect("len 16")))
    }

    fn expect_end(&mut self) -> CliResult {
        if self.offset != self.bytes.len() {
            return Err(stale(format!(
                "persistent binary multi sidecar {} has trailing bytes; rebuild the vault search indexes",
                self.index_rel
            )));
        }
        Ok(())
    }
}
