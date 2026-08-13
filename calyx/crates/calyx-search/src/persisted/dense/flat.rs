use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use calyx_core::{CxId, SlotId, SlotVector};
use calyx_sextant::index::{IndexSearchHit, ranked};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::cosine;
use crate::error::CliResult;
use crate::persisted::{HashingReader, SearchIndexEntry, stale};

#[path = "flat/writer.rs"]
mod writer;
pub(in crate::persisted) use writer::StreamingWriter;
pub(super) use writer::write;

const FORMAT: &str = "calyx-search-flat-dense-v1";
const MAGIC: &[u8; 16] = b"CALYXFLATDENSE01";
const SEARCH_BATCH_ROWS: usize = 8_192;

pub(super) fn search(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
    query: &SlotVector,
    k: usize,
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<IndexSearchHit>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let SlotVector::Dense { dim, data } = query else {
        return Err(stale(format!(
            "persistent flat dense search slot {slot} received non-dense query"
        )));
    };
    let mut scored = Vec::new();
    with_verified_rows(vault_dir, entry, slot, |header, row_bytes, batch| {
        if header.dim != *dim {
            return Err(stale(format!(
                "persistent flat dense slot {slot} index dim {} != query dim {dim}; reingest/backfill the vault",
                header.dim
            )));
        }
        let mut batch_scores = batch
            .par_chunks_exact(row_bytes)
            .map_init(
                || vec![0.0_f32; data.len()],
                |decoded, row| {
                    let cx_id = decode_id(row);
                    if candidates.is_some_and(|allowed| !allowed.contains(&cx_id)) {
                        return None;
                    }
                    decode_values(&row[16..], decoded);
                    Some((cx_id, cosine(data, decoded)))
                },
            )
            .filter_map(|scored| scored)
            .collect::<Vec<_>>();
        scored.append(&mut batch_scores);
        retain_best(&mut scored, k);
        Ok(())
    })?;
    scored.sort_unstable_by(compare_scores);
    Ok(ranked(scored))
}

pub(super) fn ids(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
) -> CliResult<Vec<CxId>> {
    let mut ids = Vec::with_capacity(entry.len);
    with_verified_rows(vault_dir, entry, slot, |_header, row_bytes, batch| {
        ids.extend(batch.chunks_exact(row_bytes).map(decode_id));
        Ok(())
    })?;
    Ok(ids)
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
) -> CliResult {
    with_verified_rows(vault_dir, entry, slot, |_header, _row_bytes, _batch| Ok(()))
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    format: String,
    slot: u16,
    dim: u32,
    base_seq: u64,
    len: usize,
}

fn write_header(writer: &mut impl Write, header: &Header) -> CliResult {
    writer.write_all(MAGIC)?;
    let encoded = bincode::serde::encode_to_vec(header, bincode::config::standard())
        .map_err(|error| stale(format!("encode flat dense header failed: {error}")))?;
    writer.write_all(&(encoded.len() as u32).to_le_bytes())?;
    writer.write_all(&encoded)?;
    Ok(())
}

fn with_verified_rows<F>(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
    mut visit: F,
) -> CliResult
where
    F: FnMut(&Header, usize, &[u8]) -> CliResult,
{
    entry.require_kind("flat_dense", slot)?;
    let path = vault_dir.join(entry.require_index_rel(slot)?);
    let file = File::open(&path)?;
    let file_len = file.metadata()?.len();
    let mut reader = HashingReader::new(BufReader::new(file));
    let (header, header_len) = read_header(&mut reader, &path)?;
    validate_header(&header, entry, slot)?;
    let row_bytes = row_bytes(header.dim, slot)?;
    validate_size(file_len, header_len, row_bytes, &header, slot, &path)?;
    let batch_rows = SEARCH_BATCH_ROWS.min(header.len.max(1));
    let batch_bytes = row_bytes.checked_mul(batch_rows).ok_or_else(|| {
        stale(format!(
            "persistent flat dense slot {slot} batch size overflow"
        ))
    })?;
    let mut buffer = vec![0_u8; batch_bytes];
    let mut remaining = header.len;
    let mut last_cx_id = None;
    while remaining > 0 {
        let rows = remaining.min(batch_rows);
        let bytes = row_bytes.checked_mul(rows).ok_or_else(|| {
            stale(format!(
                "persistent flat dense slot {slot} page size overflow"
            ))
        })?;
        reader.read_exact(&mut buffer[..bytes])?;
        validate_rows(
            &buffer[..bytes],
            row_bytes,
            header.dim,
            slot,
            &path,
            &mut last_cx_id,
        )?;
        visit(&header, row_bytes, &buffer[..bytes])?;
        remaining -= rows;
    }
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has trailing bytes; rebuild the vault search indexes",
            path.display()
        )));
    }
    let actual = reader.into_sha256();
    let expected = entry.require_sha256(slot)?;
    if actual != expected {
        return Err(stale(format!(
            "persistent flat dense sidecar sha256 {actual} != manifest {expected}; rebuild the vault search indexes"
        )));
    }
    Ok(())
}

fn read_header(reader: &mut impl Read, path: &Path) -> CliResult<(Header, usize)> {
    let mut magic = [0_u8; 16];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has invalid magic; rebuild the vault search indexes",
            path.display()
        )));
    }
    let mut raw_len = [0_u8; 4];
    reader.read_exact(&mut raw_len)?;
    let header_len = u32::from_le_bytes(raw_len) as usize;
    if header_len == 0 || header_len > 64 * 1024 {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has invalid header length {header_len}; rebuild the vault search indexes",
            path.display()
        )));
    }
    let mut encoded = vec![0_u8; header_len];
    reader.read_exact(&mut encoded)?;
    let (header, consumed): (Header, usize) =
        bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).map_err(
            |error| {
                stale(format!(
                    "persistent flat dense sidecar {} header decode failed: {error}; rebuild the vault search indexes",
                    path.display()
                ))
            },
        )?;
    if consumed != encoded.len() {
        return Err(stale(format!(
            "persistent flat dense sidecar {} header consumed {consumed} of {} bytes; rebuild the vault search indexes",
            path.display(),
            encoded.len()
        )));
    }
    Ok((header, header_len))
}

fn validate_header(header: &Header, entry: &SearchIndexEntry, slot: SlotId) -> CliResult {
    if header.format != FORMAT {
        return Err(stale(format!(
            "persistent flat dense sidecar has format {}; expected {FORMAT}",
            header.format
        )));
    }
    if header.slot != slot.get() || entry.slot != slot.get() {
        return Err(stale(format!(
            "persistent flat dense sidecar slot {} / entry slot {} != query slot {}",
            header.slot,
            entry.slot,
            slot.get()
        )));
    }
    let entry_dim = entry.require_dim(slot)?;
    if header.dim != entry_dim {
        return Err(stale(format!(
            "persistent flat dense sidecar dim {} != manifest dim {entry_dim}; rebuild the vault search indexes",
            header.dim
        )));
    }
    if header.base_seq != entry.built_at_seq {
        return Err(stale(format!(
            "persistent flat dense sidecar seq {} != manifest seq {}; rebuild the vault search indexes",
            header.base_seq, entry.built_at_seq
        )));
    }
    if header.len != entry.len {
        return Err(stale(format!(
            "persistent flat dense sidecar row len {} != manifest len {}; rebuild the vault search indexes",
            header.len, entry.len
        )));
    }
    Ok(())
}

fn row_bytes(dim: u32, slot: SlotId) -> CliResult<usize> {
    16usize.checked_add(dim as usize * 4).ok_or_else(|| {
        stale(format!(
            "persistent flat dense slot {slot} row byte size overflow"
        ))
    })
}

fn validate_size(
    file_len: u64,
    header_len: usize,
    row_bytes: usize,
    header: &Header,
    slot: SlotId,
    path: &Path,
) -> CliResult {
    let expected_len = MAGIC
        .len()
        .checked_add(4)
        .and_then(|prefix| prefix.checked_add(header_len))
        .and_then(|prefix| prefix.checked_add(row_bytes.checked_mul(header.len)?))
        .ok_or_else(|| {
            stale(format!(
                "persistent flat dense slot {slot} file size overflow"
            ))
        })?;
    if file_len != expected_len as u64 {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has {file_len} bytes, expected {expected_len}; rebuild the vault search indexes",
            path.display()
        )));
    }
    Ok(())
}

fn validate_rows(
    bytes: &[u8],
    row_bytes: usize,
    dim: u32,
    slot: SlotId,
    path: &Path,
    last_cx_id: &mut Option<CxId>,
) -> CliResult {
    for row in bytes.chunks_exact(row_bytes) {
        let cx_id = decode_id(row);
        if last_cx_id.is_some_and(|previous| previous >= cx_id) {
            return Err(stale(format!(
                "persistent flat dense sidecar {} slot {slot} IDs are not strictly increasing: previous={last_cx_id:?}, current={cx_id}; rebuild the vault search indexes",
                path.display()
            )));
        }
        if row[16..].chunks_exact(4).take(dim as usize).any(|raw| {
            !f32::from_le_bytes(raw.try_into().expect("four-byte float chunk")).is_finite()
        }) {
            return Err(stale(format!(
                "persistent flat dense sidecar {} has non-finite value for slot {slot}; rebuild the vault search indexes",
                path.display()
            )));
        }
        *last_cx_id = Some(cx_id);
    }
    Ok(())
}

fn decode_id(row: &[u8]) -> CxId {
    CxId::from_bytes(row[..16].try_into().expect("validated flat dense ID width"))
}

fn decode_values(encoded: &[u8], destination: &mut [f32]) {
    for (value, raw) in destination.iter_mut().zip(encoded.chunks_exact(4)) {
        *value = f32::from_le_bytes(raw.try_into().expect("four-byte float chunk"));
    }
}

fn compare_scores(left: &(CxId, f32), right: &(CxId, f32)) -> std::cmp::Ordering {
    right
        .1
        .total_cmp(&left.1)
        .then_with(|| left.0.cmp(&right.0))
}

fn retain_best(scored: &mut Vec<(CxId, f32)>, k: usize) {
    if scored.len() > k {
        scored.select_nth_unstable_by(k, compare_scores);
        scored.truncate(k);
    }
}
