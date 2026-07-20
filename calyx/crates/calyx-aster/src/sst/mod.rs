//! Immutable SSTable writer and mmap reader.

pub mod arrow;
mod bloom;
#[path = "io.rs"]
mod io_helpers;
pub mod level;
mod page;
mod point_read;
mod reader_cache;

pub use reader_cache::{invalidate_reader, shared_reader};

/// Hard ceiling on immutable SST sources participating in one range page.
///
/// Compaction admission imports this same constant so write-side maintenance
/// cannot drift from the read-side bound it must protect.
pub const MAX_INTERSECTING_SST_PAGE_SOURCES: usize = 512;

use crate::mmap_col::MmapColumn;
use bloom::BloomFilter;
use calyx_core::{CalyxError, Result};
use std::path::{Path, PathBuf};

use io_helpers::{record_crc, section_crc};
pub(crate) use point_read::{SstPageReader, SstPointReader, SstStreamingReader};

const MAGIC: &[u8; 4] = b"CXS1";
const LEGACY_VERSION: u32 = 1;
const VERSION: u32 = 2;
const HEADER_LEN: usize = 32;
const RECORD_HEADER_LEN: usize = 12;
const INDEX_ENTRY_FIXED_LEN: usize = 12;

/// Metadata returned after writing an SSTable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstSummary {
    pub path: PathBuf,
    pub entries: usize,
    pub bytes: u64,
    pub index_offset: u64,
    pub bloom_offset: u64,
}

/// A key/value row read from an SSTable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// A key-only SST row view used by latest-read scans that must respect
/// tombstones without cloning record values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstKeyState {
    pub key: Vec<u8>,
    pub is_tombstone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    key: Vec<u8>,
    offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SstLookupMetadata {
    pub(crate) first_key: Vec<u8>,
    pub(crate) last_key: Vec<u8>,
    bloom: BloomFilter,
    index: Vec<IndexEntry>,
}

impl SstLookupMetadata {
    pub(crate) fn record_offset(&self, key: &[u8]) -> Option<u64> {
        self.index
            .binary_search_by(|entry| entry.key.as_slice().cmp(key))
            .ok()
            .map(|position| self.index[position].offset)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.index.iter().map(|entry| entry.key.as_slice())
    }

    pub(crate) fn lower_bound(&self, key: &[u8], exclusive: bool) -> usize {
        if exclusive {
            self.index
                .partition_point(|entry| entry.key.as_slice() <= key)
        } else {
            self.index
                .partition_point(|entry| entry.key.as_slice() < key)
        }
    }

    pub(crate) fn key_at(&self, position: usize) -> Option<&[u8]> {
        self.index.get(position).map(|entry| entry.key.as_slice())
    }

    pub(crate) fn entry_at(&self, position: usize) -> Option<(&[u8], u64)> {
        self.index
            .get(position)
            .map(|entry| (entry.key.as_slice(), entry.offset))
    }

    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn estimated_heap_bytes(&self) -> usize {
        self.index
            .capacity()
            .saturating_mul(std::mem::size_of::<IndexEntry>())
            .saturating_add(self.index.iter().fold(0_usize, |bytes, entry| {
                bytes.saturating_add(entry.key.capacity())
            }))
            .saturating_add(self.first_key.capacity())
            .saturating_add(self.last_key.capacity())
            .saturating_add(self.bloom.estimated_heap_bytes())
    }
}

/// Memory-mapped SSTable reader.
#[derive(Debug)]
pub struct SstReader {
    column: MmapColumn,
    index: Vec<IndexEntry>,
    bloom: BloomFilter,
}

/// Writes a sorted immutable SSTable. The input iterator must already be ordered.
pub fn write_sst<'a>(
    path: impl AsRef<Path>,
    entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<SstSummary> {
    let path = path.as_ref();
    let entries: Vec<_> = entries
        .into_iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
    ensure_sorted(&entries)?;

    let mut bytes = vec![0u8; HEADER_LEN];
    let mut index = Vec::with_capacity(entries.len());
    for (key, value) in &entries {
        let offset = bytes.len() as u64;
        write_record(&mut bytes, key, value)?;
        index.push(IndexEntry {
            key: key.clone(),
            offset,
        });
    }

    let index_offset = bytes.len() as u64;
    write_index(&mut bytes, &index);
    let bloom_offset = bytes.len() as u64;
    BloomFilter::from_keys(entries.iter().map(|(key, _)| key.as_slice())).encode(&mut bytes);
    let body_crc = section_crc(&bytes[HEADER_LEN..]);
    write_header(
        &mut bytes,
        entries.len() as u32,
        index_offset,
        bloom_offset,
        body_crc,
    );

    crate::fsync::write_atomic_create_new(path, &bytes, "SST")?;

    Ok(SstSummary {
        path: path.to_path_buf(),
        entries: entries.len(),
        bytes: bytes.len() as u64,
        index_offset,
        bloom_offset,
    })
}

impl SstReader {
    /// Opens an SSTable through mmap.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let column = MmapColumn::open(path.as_ref())?;
        let bytes = column.as_bytes();
        let header = read_header(bytes)?;
        let index = read_index(
            bytes,
            header.entries,
            header.index_offset,
            header.bloom_offset,
        )?;
        let bloom_bytes = bytes
            .get(header.bloom_offset as usize..)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("SST bloom offset out of bounds"))?;
        let bloom = BloomFilter::decode(bloom_bytes)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("invalid SST bloom filter"))?;
        Ok(Self {
            column,
            index,
            bloom,
        })
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if !self.bloom.may_contain(key) {
            return Ok(None);
        }
        let Ok(position) = self
            .index
            .binary_search_by(|entry| entry.key.as_slice().cmp(key))
        else {
            return Ok(None);
        };
        Ok(Some(
            read_record(self.column.as_bytes(), self.index[position].offset)?.value,
        ))
    }

    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<Vec<SstEntry>> {
        let start_at = self
            .index
            .partition_point(|entry| entry.key.as_slice() < start);
        let mut rows = Vec::new();
        for entry in &self.index[start_at..] {
            if entry.key.as_slice() >= end {
                break;
            }
            rows.push(read_record(self.column.as_bytes(), entry.offset)?);
        }
        Ok(rows)
    }

    /// Reads at most one record: the greatest key in the requested bound.
    pub(crate) fn predecessor(
        &self,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Result<Option<SstEntry>> {
        let position = self.index.partition_point(|entry| {
            if inclusive {
                entry.key.as_slice() <= upper
            } else {
                entry.key.as_slice() < upper
            }
        });
        let Some(entry) = position
            .checked_sub(1)
            .and_then(|index| self.index.get(index))
        else {
            return Ok(None);
        };
        if entry.key.as_slice() < start {
            return Ok(None);
        }
        read_record(self.column.as_bytes(), entry.offset).map(Some)
    }

    pub fn range_key_states(&self, start: &[u8], end: &[u8]) -> Result<Vec<SstKeyState>> {
        self.range_key_states_until(start, Some(end))
    }

    pub fn range_key_states_until(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Vec<SstKeyState>> {
        let start_at = self
            .index
            .partition_point(|entry| entry.key.as_slice() < start);
        let mut rows = Vec::new();
        for entry in &self.index[start_at..] {
            if end.is_some_and(|end| entry.key.as_slice() >= end) {
                break;
            }
            let record = read_record_ref(self.column.as_bytes(), entry.offset)?;
            rows.push(SstKeyState {
                key: record.key.to_vec(),
                is_tombstone: crate::mvcc::is_tombstone_value(record.value),
            });
        }
        Ok(rows)
    }

    pub fn iter(&self) -> Result<Vec<SstEntry>> {
        self.index
            .iter()
            .map(|entry| read_record(self.column.as_bytes(), entry.offset))
            .collect()
    }

    pub(crate) fn iter_with_offsets(&self) -> Result<Vec<(u64, SstEntry)>> {
        self.index
            .iter()
            .map(|entry| {
                Ok((
                    entry.offset,
                    read_record(self.column.as_bytes(), entry.offset)?,
                ))
            })
            .collect()
    }

    pub fn bloom_may_contain(&self, key: &[u8]) -> bool {
        self.bloom.may_contain(key)
    }

    /// First and last key stored in this SST, or `None` for an empty file.
    pub fn key_range(&self) -> Option<(&[u8], &[u8])> {
        Some((
            self.index.first()?.key.as_slice(),
            self.index.last()?.key.as_slice(),
        ))
    }

    pub(crate) fn lookup_metadata(&self) -> Option<SstLookupMetadata> {
        let first_key = self.index.first()?.key.clone();
        let last_key = self.index.last()?.key.clone();
        Some(SstLookupMetadata {
            first_key,
            last_key,
            bloom: self.bloom.clone(),
            index: self.index.clone(),
        })
    }

    /// Clones the index from an already whole-file-validated reader.
    ///
    /// Unlike [`Self::lookup_metadata`], an empty vector is a valid result for
    /// an empty SST. Streaming compaction must distinguish that state from a
    /// missing or unvalidated index.
    fn validated_index(&self) -> Vec<IndexEntry> {
        self.index.clone()
    }
}

struct SstRecordRef<'a> {
    key: &'a [u8],
    value: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
struct Header {
    entries: u32,
    index_offset: u64,
    bloom_offset: u64,
}

fn ensure_sorted(entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
    if entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(CalyxError::aster_corrupt_shard(
            "SST entries must be strictly sorted by key",
        ));
    }
    Ok(())
}

fn write_record(out: &mut Vec<u8>, key: &[u8], value: &[u8]) -> Result<()> {
    let key_len = u32::try_from(key.len())
        .map_err(|_| CalyxError::disk_pressure("SST key length exceeds u32"))?;
    let value_len = u32::try_from(value.len())
        .map_err(|_| CalyxError::disk_pressure("SST value length exceeds u32"))?;
    let crc = record_crc(key, value);
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    Ok(())
}

fn read_record(bytes: &[u8], offset: u64) -> Result<SstEntry> {
    let record = read_record_ref(bytes, offset)?;
    Ok(SstEntry {
        key: record.key.to_vec(),
        value: record.value.to_vec(),
    })
}

fn read_record_ref(bytes: &[u8], offset: u64) -> Result<SstRecordRef<'_>> {
    let offset = offset as usize;
    let header = bytes
        .get(offset..offset + RECORD_HEADER_LEN)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST record header out of bounds"))?;
    let key_len = u32::from_le_bytes(header[0..4].try_into().expect("key len")) as usize;
    let value_len = u32::from_le_bytes(header[4..8].try_into().expect("value len")) as usize;
    let expected_crc = u32::from_le_bytes(header[8..12].try_into().expect("record crc"));
    let key_start = offset + RECORD_HEADER_LEN;
    let value_start = key_start + key_len;
    let value_end = value_start + value_len;
    let key = bytes
        .get(key_start..value_start)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST key out of bounds"))?;
    let value = bytes
        .get(value_start..value_end)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST value out of bounds"))?;
    let actual_crc = record_crc(key, value);
    if actual_crc != expected_crc {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "SST record crc mismatch at {offset}: expected {expected_crc:08x}, got {actual_crc:08x}"
        )));
    }
    Ok(SstRecordRef { key, value })
}

fn write_index(out: &mut Vec<u8>, index: &[IndexEntry]) {
    for entry in index {
        out.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
        out.extend_from_slice(&entry.offset.to_le_bytes());
        out.extend_from_slice(&entry.key);
    }
}

fn read_index(
    bytes: &[u8],
    entries: u32,
    index_offset: u64,
    bloom_offset: u64,
) -> Result<Vec<IndexEntry>> {
    let mut offset = index_offset as usize;
    let end = bloom_offset as usize;
    let mut index = Vec::with_capacity(entries as usize);
    for _ in 0..entries {
        let fixed = bytes
            .get(offset..offset + INDEX_ENTRY_FIXED_LEN)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("SST index entry out of bounds"))?;
        let key_len = u32::from_le_bytes(fixed[0..4].try_into().expect("index key len")) as usize;
        let record_offset = u64::from_le_bytes(fixed[4..12].try_into().expect("record offset"));
        offset += INDEX_ENTRY_FIXED_LEN;
        let key = bytes
            .get(offset..offset + key_len)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("SST index key out of bounds"))?
            .to_vec();
        offset += key_len;
        index.push(IndexEntry {
            key,
            offset: record_offset,
        });
    }
    if offset != end {
        return Err(CalyxError::aster_corrupt_shard("SST index length mismatch"));
    }
    if index.windows(2).any(|pair| pair[0].key >= pair[1].key) {
        return Err(CalyxError::aster_corrupt_shard(
            "SST index keys must be strictly sorted",
        ));
    }
    Ok(index)
}

fn write_header(
    bytes: &mut [u8],
    entries: u32,
    index_offset: u64,
    bloom_offset: u64,
    body_crc: u32,
) {
    bytes[0..4].copy_from_slice(MAGIC);
    bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
    bytes[8..12].copy_from_slice(&entries.to_le_bytes());
    bytes[12..20].copy_from_slice(&index_offset.to_le_bytes());
    bytes[20..28].copy_from_slice(&bloom_offset.to_le_bytes());
    bytes[28..32].copy_from_slice(&body_crc.to_le_bytes());
}

fn read_header(bytes: &[u8]) -> Result<Header> {
    let header = bytes
        .get(0..HEADER_LEN)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST header missing"))?;
    if &header[0..4] != MAGIC {
        return Err(CalyxError::aster_corrupt_shard("SST magic mismatch"));
    }
    let version = u32::from_le_bytes(header[4..8].try_into().expect("version"));
    if version != VERSION && version != LEGACY_VERSION {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "unsupported SST version {version}"
        )));
    }
    let entries = u32::from_le_bytes(header[8..12].try_into().expect("entries"));
    let index_offset = u64::from_le_bytes(header[12..20].try_into().expect("index offset"));
    let bloom_offset = u64::from_le_bytes(header[20..28].try_into().expect("bloom offset"));
    let len = bytes.len() as u64;
    if index_offset < HEADER_LEN as u64
        || index_offset > len
        || bloom_offset < index_offset
        || bloom_offset > len
    {
        return Err(CalyxError::aster_corrupt_shard(
            "SST header offsets out of bounds",
        ));
    }
    if version >= VERSION {
        let expected_crc = u32::from_le_bytes(header[28..32].try_into().expect("body crc"));
        let actual_crc = section_crc(
            bytes
                .get(HEADER_LEN..)
                .ok_or_else(|| CalyxError::aster_corrupt_shard("SST body missing"))?,
        );
        if actual_crc != expected_crc {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST body crc mismatch: expected {expected_crc:08x}, got {actual_crc:08x}"
            )));
        }
    }
    Ok(Header {
        entries,
        index_offset,
        bloom_offset,
    })
}
