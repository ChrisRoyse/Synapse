use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use calyx_core::{CxId, SlotId, SlotVector};
use calyx_sextant::index::{IndexSearchHit, ranked};
use serde::{Deserialize, Serialize};

use super::{DenseSlotRows, cosine};
use crate::error::CliResult;
use crate::persisted::pinned::{self, PinKey};
use crate::persisted::{SearchIndexEntry, rel, sha256_hex, stale, write_atomic_hashed};

const FORMAT: &str = "calyx-search-flat-dense-v1";
const MAGIC: &[u8; 16] = b"CALYXFLATDENSE01";
const DEFAULT_MAX_ROWS: usize = 32_768;
const PIN_KIND: &str = "flat_dense";

pub(super) fn should_use_index(row_count: usize) -> bool {
    row_count <= DEFAULT_MAX_ROWS
}

pub(super) fn write(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    rows: DenseSlotRows,
    base_seq: u64,
) -> CliResult<SearchIndexEntry> {
    let path = root.join(format!(
        "slot_{:05}_seq_{base_seq:020}_n_{:010}.flatdense.bin",
        slot.get(),
        rows.rows.len()
    ));
    let header = Header {
        format: FORMAT.to_string(),
        slot: slot.get(),
        dim: rows.dim,
        base_seq,
        len: rows.rows.len(),
    };
    let sha256 = write_atomic_hashed(&path, |writer| write_sidecar(writer, &header, &rows.rows))?;
    Ok(SearchIndexEntry::flat_dense(
        slot,
        rows.dim,
        rows.rows.len(),
        base_seq,
        rel(vault_dir, &path)?,
        sha256,
    ))
}

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
    let index = pinned_index(vault_dir, entry, slot)?;
    if index.header.dim != *dim {
        return Err(stale(format!(
            "persistent flat dense slot {slot} index dim {} != query dim {dim}; reingest/backfill the vault",
            index.header.dim
        )));
    }
    let mut scored = index
        .rows
        .iter()
        .filter(|(cx_id, _)| candidates.is_none_or(|allowed| allowed.contains(cx_id)))
        .map(|(cx_id, values)| (*cx_id, cosine(data, values)))
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
    });
    scored.truncate(k);
    Ok(ranked(scored))
}

pub(super) fn ids(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
) -> CliResult<Vec<CxId>> {
    Ok(pinned_index(vault_dir, entry, slot)?
        .rows
        .iter()
        .map(|(cx_id, _)| *cx_id)
        .collect())
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    format: String,
    slot: u16,
    dim: u32,
    base_seq: u64,
    len: usize,
}

#[derive(Debug)]
struct Index {
    header: Header,
    rows: Vec<(CxId, Vec<f32>)>,
}

type FlatPinCache = Mutex<BTreeMap<(String, u16), (String, Arc<Index>)>>;

fn cache() -> &'static FlatPinCache {
    static CACHE: OnceLock<FlatPinCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Verify-once-then-pin: the flat dense sidecar is fully read, hashed, and
/// validated on first use per manifest generation (keyed by the manifest
/// entry sha256); cache hits still fail closed on seq drift between the
/// pinned header and the manifest entry being served.
fn pinned_index(vault_dir: &Path, entry: &SearchIndexEntry, slot: SlotId) -> CliResult<Arc<Index>> {
    let entry_sha256 = entry.require_sha256(slot)?.to_string();
    let cache_key = (pinned::canonical_vault_dir(vault_dir)?, slot.get());
    {
        let cache = cache().lock().expect("flat dense pin cache poisoned");
        if let Some((pinned_sha, index)) = cache.get(&cache_key)
            && *pinned_sha == entry_sha256
        {
            if index.header.base_seq != entry.built_at_seq {
                return Err(stale(format!(
                    "persistent flat dense sidecar seq {} != manifest seq {}; rebuild the vault search indexes",
                    index.header.base_seq, entry.built_at_seq
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
    let index = Arc::new(read(vault_dir, entry, slot)?);
    let pin_key = PinKey::new(vault_dir, slot.get(), PIN_KIND)?;
    pinned::reserve(&pin_key, sidecar_bytes)?;
    let mut cache = cache().lock().expect("flat dense pin cache poisoned");
    cache.insert(cache_key, (entry_sha256, Arc::clone(&index)));
    Ok(index)
}

fn write_sidecar(
    writer: &mut impl Write,
    header: &Header,
    rows: &[(CxId, Vec<f32>)],
) -> CliResult<()> {
    writer.write_all(MAGIC)?;
    let header = bincode::serde::encode_to_vec(header, bincode::config::standard())
        .map_err(|error| stale(format!("encode flat dense header failed: {error}")))?;
    writer.write_all(&(header.len() as u32).to_le_bytes())?;
    writer.write_all(&header)?;
    for (cx_id, values) in rows {
        writer.write_all(cx_id.as_bytes())?;
        for value in values {
            writer.write_all(&value.to_le_bytes())?;
        }
    }
    Ok(())
}

fn read(vault_dir: &Path, entry: &SearchIndexEntry, slot: SlotId) -> CliResult<Index> {
    entry.require_kind("flat_dense", slot)?;
    let path = vault_dir.join(entry.require_index_rel(slot)?);
    let bytes = fs::read(&path)?;
    let actual = sha256_hex(&bytes);
    let expected = entry.require_sha256(slot)?;
    if actual != expected {
        return Err(stale(format!(
            "persistent flat dense sidecar sha256 {actual} != manifest {expected}; rebuild the vault search indexes"
        )));
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let mut magic = [0u8; 16];
    cursor.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has invalid magic; rebuild the vault search indexes",
            path.display()
        )));
    }
    let header = read_header(&mut cursor, &path)?;
    validate_header(&header, entry, slot)?;
    validate_size(&cursor, &path, &header, slot)?;
    read_rows(cursor, header, slot, &path)
}

pub(super) fn validate_entry(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
) -> CliResult {
    let _ = read(vault_dir, entry, slot)?;
    Ok(())
}

fn read_header(cursor: &mut std::io::Cursor<Vec<u8>>, path: &Path) -> CliResult<Header> {
    let mut header_len = [0u8; 4];
    cursor.read_exact(&mut header_len)?;
    let header_len = u32::from_le_bytes(header_len) as usize;
    if header_len == 0 || header_len > 64 * 1024 {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has invalid header length {header_len}; rebuild the vault search indexes",
            path.display()
        )));
    }
    let mut header_bytes = vec![0u8; header_len];
    cursor.read_exact(&mut header_bytes)?;
    let (header, consumed): (Header, usize) =
        bincode::serde::decode_from_slice(&header_bytes, bincode::config::standard()).map_err(
            |error| {
                stale(format!(
                    "persistent flat dense sidecar {} header decode failed: {error}; rebuild the vault search indexes",
                    path.display()
                ))
            },
        )?;
    if consumed != header_bytes.len() {
        return Err(stale(format!(
            "persistent flat dense sidecar {} header consumed {consumed} of {} bytes; rebuild the vault search indexes",
            path.display(),
            header_bytes.len()
        )));
    }
    Ok(header)
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

fn validate_size(
    cursor: &std::io::Cursor<Vec<u8>>,
    path: &Path,
    header: &Header,
    slot: SlotId,
) -> CliResult {
    let header_len = cursor.position() as usize - MAGIC.len() - 4;
    let row_bytes = 16usize
        .checked_add(header.dim as usize * 4)
        .ok_or_else(|| {
            stale(format!(
                "persistent flat dense slot {slot} row byte size overflow"
            ))
        })?;
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
    if cursor.get_ref().len() != expected_len {
        return Err(stale(format!(
            "persistent flat dense sidecar {} has {} bytes, expected {expected_len}; rebuild the vault search indexes",
            path.display(),
            cursor.get_ref().len()
        )));
    }
    Ok(())
}

fn read_rows(
    mut cursor: std::io::Cursor<Vec<u8>>,
    header: Header,
    slot: SlotId,
    path: &Path,
) -> CliResult<Index> {
    let mut rows = Vec::with_capacity(header.len);
    for _ in 0..header.len {
        let mut id = [0u8; 16];
        cursor.read_exact(&mut id)?;
        let mut values = Vec::with_capacity(header.dim as usize);
        for _ in 0..header.dim {
            let mut raw = [0u8; 4];
            cursor.read_exact(&mut raw)?;
            let value = f32::from_le_bytes(raw);
            if !value.is_finite() {
                return Err(stale(format!(
                    "persistent flat dense sidecar {} has non-finite value for slot {slot}; rebuild the vault search indexes",
                    path.display()
                )));
            }
            values.push(value);
        }
        rows.push((CxId::from_bytes(id), values));
    }
    Ok(Index { header, rows })
}
