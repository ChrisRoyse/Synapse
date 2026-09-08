//! Immutable SSTable writer and mmap reader.

pub mod arrow;
mod bloom;
#[path = "io.rs"]
mod io_helpers;
pub mod level;
pub(crate) mod page;
mod point_read;
mod reader_cache;

pub use reader_cache::{
    SstReaderCacheStatus, invalidate_reader, invalidate_reader_canonical, reader_cache_status,
    shared_reader,
};

/// Hard ceiling on immutable SST sources participating in one range page.
///
/// Compaction admission imports this same constant so write-side maintenance
/// cannot drift from the read-side bound it must protect.
pub const MAX_INTERSECTING_SST_PAGE_SOURCES: usize = 512;

/// Immutable SST files one *admitted* router put can still add to a CF before
/// the next admission check runs.
///
/// A single put seals at most twice: once to relieve memtable backpressure,
/// and once more when the retried write reports `flush_triggered`. Each seal
/// writes one immutable SST.
pub const MAX_SST_FILES_PER_ADMITTED_PUT: usize = 2;

/// Highest live source count at which ordinary ingest is still admitted.
///
/// Admission has to stop [`MAX_SST_FILES_PER_ADMITTED_PUT`] short of the hard
/// limit, or the write admitted *because* the CF was under the bound is itself
/// what carries it over: a CF at 511 admits, seals twice, and lands at 513 --
/// above [`MAX_INTERSECTING_SST_PAGE_SOURCES`], which is what range paging
/// cannot exceed. This vault reached exactly 513 sources on `base` and `ledger`
/// that way, and every range scan over them then failed closed with
/// `CALYX_ASTER_SST_SEQUENTIAL_SOURCE_LIMIT_EXCEEDED` -- including the GC
/// reachability census, which is what schedules the compaction that would have
/// reduced them.
pub const INGEST_ADMISSION_CEILING_SST_PAGE_SOURCES: usize =
    MAX_INTERSECTING_SST_PAGE_SOURCES - MAX_SST_FILES_PER_ADMITTED_PUT;

/// Upper bound on decoded key+value bytes a single [`SstReader`] range scan will
/// materialize into memory before failing closed.
///
/// SSTs target `DEFAULT_COMPACTION_TARGET_BYTES` (64 MiB) on disk, so a single
/// reader materializing more than 1 GiB of decoded bytes is pathological — a
/// runaway/oversized value, a corrupt record count, or an unexpectedly wide
/// scan. Left unbounded, the accumulating `Vec` drives allocation into
/// `handle_alloc_error`, which calls `abort()` (`__fastfail`, exit 0xC0000409)
/// and takes down the whole process instead of returning an error the LSM query
/// path can quarantine. Under rayon fan-out (`Level::range` scans every
/// intersecting SST in parallel) the risk is multiplied across worker threads.
/// See issue #1809: two daemon crashes at the shared abort trampoline, both with
/// `SstReader::range` -> `RawVec::grow_one` -> `handle_alloc_error` on the
/// faulting rayon worker.
/// Hard ceiling for one materialized SST row/range/level merge.
///
/// The former 1 GiB limit allowed one malformed or unbounded request to consume
/// the daemon's entire supported memory envelope before the allocation was
/// refused. Normal SSTs target 64 MiB, so 128 MiB preserves room for one full
/// input plus merge/output bookkeeping while keeping the process comfortably
/// below the 1 GiB host budget.
pub(super) const MAX_RANGE_SCAN_BYTES: usize = 128 << 20;

/// Fail-closed error for a range scan that would exceed [`MAX_RANGE_SCAN_BYTES`].
fn scan_budget_exceeded(scanned: usize, next_record: usize) -> CalyxError {
    CalyxError {
        code: "CALYX_ASTER_SCAN_MEMORY_BUDGET",
        message: format!(
            "SST range scan exceeded the {MAX_RANGE_SCAN_BYTES}-byte materialization budget \
             (accumulated={scanned} bytes, next_record={next_record} bytes); refusing to \
             allocate further to avoid an allocator abort"
        ),
        remediation: "narrow the key range or split the query; a single-SST scan this large \
                      indicates a runaway value or corrupt record and is refused fail-closed",
    }
}

/// Fail-closed error when growing the scan buffer fails allocation.
///
/// Converts what would otherwise be an infallible `Vec` growth (aborting the
/// process via `handle_alloc_error`) into a propagable structured error.
pub(super) fn scan_reserve_failed(err: std::collections::TryReserveError) -> CalyxError {
    CalyxError {
        code: "CALYX_ASTER_SCAN_ALLOC",
        message: format!("SST range scan could not reserve memory for the next record: {err}"),
        remediation: "the host is out of memory for this query; narrow the range or free memory \
                      — the scan fails closed instead of aborting the daemon",
    }
}

use crate::mmap_col::MmapColumn;
use bloom::BloomFilter;
use calyx_core::{CalyxError, Result};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use io_helpers::{record_crc, section_crc};
pub(crate) use point_read::{SstPageReader, SstPointReader, SstStreamingReader};

const MAGIC: &[u8; 4] = b"CXS1";
const LEGACY_VERSION: u32 = 1;
const CHECKSUMMED_VERSION: u32 = 2;
const VERSION: u32 = 3;
const HEADER_LEN: usize = 32;
const RECORD_HEADER_LEN: usize = 12;
const COMPRESSED_RECORD_HEADER_LEN: usize = 16;
const INDEX_ENTRY_FIXED_LEN: usize = 12;
const SST_ZSTD_LEVEL: i32 = 3;
const MIN_COMPRESSION_INPUT_BYTES: usize = 256;
const MIN_COMPRESSION_SAVINGS_BYTES: usize = 32;
static SST_WRITE_VERSION: AtomicU32 = AtomicU32::new(VERSION);
static SST_WRITES_STARTED: AtomicBool = AtomicBool::new(false);

/// Selects the SST format emitted by this process before its first SST write.
///
/// Version 2 exists only as an upgrade-transaction compatibility fence: a
/// pre-commit candidate can be rolled back to a v2-only predecessor without
/// leaving unreadable v3 files behind. Readers always accept v1, v2, and v3;
/// committed current generations use v3 compression.
pub fn configure_sst_write_version(version: u32) -> Result<()> {
    if version != CHECKSUMMED_VERSION && version != VERSION {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "unsupported configured SST write version {version}; expected 2 or 3"
        )));
    }
    if SST_WRITES_STARTED.load(Ordering::Acquire) {
        return Err(CalyxError::aster_corrupt_shard(
            "SST write version cannot change after the first process write",
        ));
    }
    SST_WRITE_VERSION.store(version, Ordering::Release);
    Ok(())
}

/// Metadata returned after writing an SSTable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstSummary {
    pub path: PathBuf,
    pub entries: usize,
    pub bytes: u64,
    pub index_offset: u64,
    pub bloom_offset: u64,
    pub first_key: Option<Vec<u8>>,
    pub last_key: Option<Vec<u8>>,
}

/// Small, allocation-bounded key range used to reject immutable files that
/// cannot answer a point or range read.
///
/// Unlike [`SstLookupMetadata`], this never retains every key in the SST. A
/// cold-open router can therefore prune thousands of immutable sources without
/// recreating the multi-gigabyte decoded-index resident set that issue #2239
/// exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SstBounds {
    pub(crate) first_key: Vec<u8>,
    pub(crate) last_key: Vec<u8>,
    /// Validated whole-key filter used before any cold index or data I/O.
    ///
    /// The on-disk filter is about two bytes per key, while the decoded index
    /// retains every key plus offsets and allocation overhead. Keeping this
    /// compact negative index prevents a point miss from reopening and
    /// walking every overlapping SST without restoring corpus-sized indexes.
    bloom: Arc<BloomFilter>,
    /// Allocation-bounded seek points into the variable-length on-disk index.
    ///
    /// Cold readers use these to start within one bounded index interval of a
    /// requested key. Retaining every decoded key made memory scale with the
    /// lifetime corpus; retaining no seek points made a near-tail lookup replay
    /// the complete historical index. The bounds scan already validates every
    /// index entry, so recording a capped subset adds no extra disk pass.
    pub(crate) sparse_index: Vec<SstSparseIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SstSparseIndexEntry {
    pub(crate) key: Vec<u8>,
    pub(crate) index_offset: u64,
    pub(crate) ordinal: usize,
    pub(crate) record_offset: u64,
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
    bloom: Arc<BloomFilter>,
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
    version: u32,
    index: Vec<IndexEntry>,
    bloom: BloomFilter,
}

/// Writes a sorted immutable SSTable. The input iterator must already be ordered.
pub fn write_sst<'a>(
    path: impl AsRef<Path>,
    entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> Result<SstSummary> {
    SST_WRITES_STARTED.store(true, Ordering::Release);
    let write_version = SST_WRITE_VERSION.load(Ordering::Acquire);
    let path = path.as_ref();
    let entries: Vec<_> = entries
        .into_iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect();
    let (bytes, index_offset, bloom_offset) = encode_sst(&entries, write_version)?;
    crate::fsync::write_atomic_create_new(path, &bytes, "SST")?;

    Ok(SstSummary {
        path: path.to_path_buf(),
        entries: entries.len(),
        bytes: bytes.len() as u64,
        index_offset,
        bloom_offset,
        first_key: entries.first().map(|(key, _)| key.clone()),
        last_key: entries.last().map(|(key, _)| key.clone()),
    })
}

fn encode_sst(entries: &[(Vec<u8>, Vec<u8>)], write_version: u32) -> Result<(Vec<u8>, u64, u64)> {
    ensure_sorted(entries)?;

    let mut bytes = vec![0u8; HEADER_LEN];
    let mut index = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let offset = bytes.len() as u64;
        write_record(&mut bytes, key, value, write_version)?;
        index.push(IndexEntry {
            key: key.clone(),
            offset,
        });
    }

    let index_offset = bytes.len() as u64;
    write_index(&mut bytes, &index);
    let bloom_offset = bytes.len() as u64;
    BloomFilter::from_keys(entries.iter().map(|(key, _)| key.as_slice()))?.encode(&mut bytes)?;
    let body_crc = section_crc(&bytes[HEADER_LEN..]);
    write_header(
        &mut bytes,
        entries.len() as u32,
        index_offset,
        bloom_offset,
        body_crc,
        write_version,
    );

    Ok((bytes, index_offset, bloom_offset))
}

/// Atomically rewrites every v3 SST below a vault root into the v2 checksummed
/// representation while preserving the complete ordered row stream.
///
/// This is the rollback half of the v3 compression migration contract.  A
/// pre-commit candidate normally emits v2 directly; this operation repairs a
/// vault touched by an older candidate that emitted v3 before authority
/// transfer. Each file is fully CRC/decompression validated, encoded in
/// memory under the ordinary 128 MiB SST scan ceiling, atomically replaced,
/// reopened, and compared row-for-row before the next file is touched.
pub fn downgrade_v3_ssts_to_v2(vault_root: impl AsRef<Path>) -> Result<(u64, u64, u64)> {
    if SST_WRITES_STARTED.swap(true, Ordering::AcqRel) {
        return Err(CalyxError::aster_corrupt_shard(
            "SST downgrade must run before any process SST write",
        ));
    }
    SST_WRITE_VERSION.store(CHECKSUMMED_VERSION, Ordering::Release);
    let root = vault_root.as_ref();
    let mut pending = Vec::new();
    collect_sst_paths(root, &mut pending)?;
    pending.sort();
    let mut rewritten = 0_u64;
    let mut input_bytes = 0_u64;
    let mut output_bytes = 0_u64;
    for path in pending {
        let reader = SstReader::open(&path)?;
        if reader.version != VERSION {
            continue;
        }
        let rows = reader.iter()?;
        let before = std::fs::metadata(&path)
            .map_err(|error| {
                CalyxError::disk_pressure(format!(
                    "stat SST {} before downgrade: {error}",
                    path.display()
                ))
            })?
            .len();
        let owned = rows
            .iter()
            .map(|row| (row.key.clone(), row.value.clone()))
            .collect::<Vec<_>>();
        let (encoded, _, _) = encode_sst(&owned, CHECKSUMMED_VERSION)?;
        drop(reader);
        crate::fsync::write_atomic_replace(&path, &encoded, "SST v3 to v2 downgrade")?;
        let verified = SstReader::open(&path)?;
        if verified.version != CHECKSUMMED_VERSION || verified.iter()? != rows {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST downgrade readback mismatch: {}",
                path.display()
            )));
        }
        rewritten = rewritten.saturating_add(1);
        input_bytes = input_bytes.saturating_add(before);
        output_bytes = output_bytes.saturating_add(encoded.len() as u64);
    }
    Ok((rewritten, input_bytes, output_bytes))
}

fn collect_sst_paths(root: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(root).map_err(|error| {
        CalyxError::disk_pressure(format!(
            "read SST downgrade root {}: {error}",
            root.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            CalyxError::disk_pressure(format!("read SST downgrade entry: {error}"))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            CalyxError::disk_pressure(format!(
                "read SST downgrade entry type {}: {error}",
                entry.path().display()
            ))
        })?;
        if file_type.is_symlink() {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST downgrade refuses symlink/reparse entry: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            collect_sst_paths(&entry.path(), output)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("sst"))
        {
            output.push(entry.path());
        }
    }
    Ok(())
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
        let bloom = BloomFilter::decode(bloom_bytes)?;
        Ok(Self {
            column,
            version: header.version,
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
            read_record(
                self.column.as_bytes(),
                self.index[position].offset,
                self.version,
            )?
            .value,
        ))
    }

    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<Vec<SstEntry>> {
        self.collect_range(start, Some(end))
    }

    fn collect_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<SstEntry>> {
        let mut rows = Vec::new();
        let mut scanned_bytes: usize = 0;
        self.visit_range_until(start, end, |key, value| {
            let record_bytes = materialized_entry_bytes::<SstEntry>(key, value);
            scanned_bytes = scanned_bytes.saturating_add(record_bytes);
            if scanned_bytes > MAX_RANGE_SCAN_BYTES {
                return Err(scan_budget_exceeded(scanned_bytes, record_bytes));
            }
            rows.try_reserve(1).map_err(scan_reserve_failed)?;
            rows.push(SstEntry {
                key: clone_scan_bytes(key)?,
                value: clone_scan_bytes(value)?,
            });
            Ok(())
        })?;
        Ok(rows)
    }

    /// Visits an ordered range through borrowed, CRC-validated record slices.
    /// The caller owns all retention and allocation policy; this method itself
    /// never materializes the range.
    pub(super) fn visit_range_until(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        let start_at = self
            .index
            .partition_point(|entry| entry.key.as_slice() < start);
        for entry in &self.index[start_at..] {
            if end.is_some_and(|end| entry.key.as_slice() >= end) {
                break;
            }
            let record = read_record_ref(self.column.as_bytes(), entry.offset, self.version)?;
            visit(record.key, record.value.as_ref())?;
        }
        Ok(())
    }

    pub fn range_key_states(&self, start: &[u8], end: &[u8]) -> Result<Vec<SstKeyState>> {
        self.range_key_states_until(start, Some(end))
    }

    pub fn range_key_states_until(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Vec<SstKeyState>> {
        let mut rows = Vec::new();
        let mut scanned_bytes: usize = 0;
        self.visit_range_until(start, end, |key, value| {
            let record_bytes = materialized_entry_bytes::<SstKeyState>(key, &[]);
            scanned_bytes = scanned_bytes.saturating_add(record_bytes);
            if scanned_bytes > MAX_RANGE_SCAN_BYTES {
                return Err(scan_budget_exceeded(scanned_bytes, record_bytes));
            }
            rows.try_reserve(1).map_err(scan_reserve_failed)?;
            rows.push(SstKeyState {
                key: clone_scan_bytes(key)?,
                is_tombstone: crate::mvcc::is_tombstone_value(value),
            });
            Ok(())
        })?;
        Ok(rows)
    }

    pub fn iter(&self) -> Result<Vec<SstEntry>> {
        self.collect_range(&[], None)
    }

    pub(crate) fn iter_with_offsets(&self) -> Result<Vec<(u64, SstEntry)>> {
        self.index
            .iter()
            .map(|entry| {
                Ok((
                    entry.offset,
                    read_record(self.column.as_bytes(), entry.offset, self.version)?,
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
            bloom: Arc::new(self.bloom.clone()),
            index: self.index.clone(),
        })
    }

    pub(crate) fn estimated_heap_bytes(&self) -> usize {
        self.index
            .capacity()
            .saturating_mul(std::mem::size_of::<IndexEntry>())
            .saturating_add(self.index.iter().fold(0_usize, |bytes, entry| {
                bytes.saturating_add(entry.key.capacity())
            }))
            .saturating_add(self.bloom.estimated_heap_bytes())
    }

    /// File-backed virtual address span owned by this reader's mmap.
    ///
    /// Faulted pages from this span contribute to physical working set even
    /// though they are not heap allocations, so caches must account for both.
    pub(crate) fn mapped_bytes(&self) -> usize {
        self.column.file_len()
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

/// Reads and validates only the immutable file's ordered key bounds.
///
/// The complete variable-length index is walked through a single bounded file
/// buffer so a malformed count, offset, length, or key order fails closed
/// without mapping the cold SST into the process working set or retaining an
/// allocation per row. The first and last indexed data records are then read
/// through the normal record CRC gate. Bounds are only a negative selection
/// index and never authorize serving bytes.
pub(crate) fn read_sst_bounds(path: &Path) -> Result<Option<SstBounds>> {
    read_sst_bounds_inner(path).map_err(|mut error| {
        error.message = format!(
            "read bounded SST key index {}: {}",
            path.display(),
            error.message
        );
        error
    })
}

fn read_sst_bounds_inner(path: &Path) -> Result<Option<SstBounds>> {
    let mut file =
        File::open(path).map_err(|error| sst_io_error("open SST bounds", path, error))?;
    let header = read_file_header_structure(&mut file, path)?;
    let file_len = file
        .metadata()
        .map_err(|error| sst_io_error("stat SST bloom", path, error))?
        .len();
    file.seek(SeekFrom::Start(header.index_offset))
        .map_err(|error| sst_io_error("seek SST bounds index", path, error))?;
    let mut reader = BufReader::with_capacity(SST_BOUNDS_BUFFER_BYTES, file);
    let mut offset = header.index_offset;
    let end = header.bloom_offset;
    let mut first = None::<(Vec<u8>, u64)>;
    let mut previous = Vec::<u8>::new();
    let mut current = Vec::<u8>::new();
    let mut last_offset = None::<u64>;
    let index_bytes = end.saturating_sub(header.index_offset);
    let sparse_interval = index_bytes
        .div_ceil((MAX_SST_SPARSE_INDEX_ENTRIES - 1) as u64)
        .max(SST_SPARSE_INDEX_INTERVAL_BYTES);
    let mut next_sparse_offset = header.index_offset;
    let mut sparse_index = Vec::new();
    let estimated_sparse_entries = usize::try_from(index_bytes.div_ceil(sparse_interval))
        .unwrap_or(usize::MAX)
        .saturating_add(1)
        .min(MAX_SST_SPARSE_INDEX_ENTRIES)
        .min(header.entries as usize);
    sparse_index
        .try_reserve_exact(estimated_sparse_entries)
        .map_err(scan_reserve_failed)?;
    for ordinal in 0..header.entries {
        let index_entry_offset = offset;
        let fixed_end = offset
            .checked_add(INDEX_ENTRY_FIXED_LEN as u64)
            .ok_or_else(|| {
                CalyxError::aster_corrupt_shard("SST index fixed-entry offset overflow")
            })?;
        if fixed_end > end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST index entry {ordinal} is out of bounds"
            )));
        }
        let mut fixed = [0_u8; INDEX_ENTRY_FIXED_LEN];
        read_exact_sst_stream(&mut reader, &mut fixed, path, offset, "index entry")?;
        let key_len = u32::from_le_bytes(fixed[0..4].try_into().expect("index key len")) as usize;
        let record_offset = u64::from_le_bytes(fixed[4..12].try_into().expect("record offset"));
        let key_end =
            fixed_end
                .checked_add(u64::try_from(key_len).map_err(|_| {
                    CalyxError::aster_corrupt_shard("SST index key length exceeds u64")
                })?)
                .ok_or_else(|| CalyxError::aster_corrupt_shard("SST index key offset overflow"))?;
        if key_end > end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST index key {ordinal} is out of bounds"
            )));
        }
        current.clear();
        current
            .try_reserve_exact(key_len)
            .map_err(scan_reserve_failed)?;
        current.resize(key_len, 0);
        read_exact_sst_stream(&mut reader, &mut current, path, fixed_end, "index key")?;
        if record_offset < HEADER_LEN as u64 || record_offset >= header.index_offset {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST index record offset {record_offset} is outside the data section {}..{}",
                HEADER_LEN, header.index_offset
            )));
        }
        if ordinal != 0 && previous >= current {
            return Err(CalyxError::aster_corrupt_shard(
                "SST index keys must be strictly sorted",
            ));
        }
        if first.is_none() {
            first = Some((clone_scan_bytes(&current)?, record_offset));
        }
        let is_last = ordinal.saturating_add(1) == header.entries;
        if ordinal == 0 || index_entry_offset >= next_sparse_offset || is_last {
            if sparse_index
                .last()
                .is_none_or(|entry: &SstSparseIndexEntry| entry.ordinal != ordinal as usize)
            {
                if sparse_index.len() >= MAX_SST_SPARSE_INDEX_ENTRIES {
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "SST sparse index exceeded its per-file anchor bound of {MAX_SST_SPARSE_INDEX_ENTRIES}"
                    )));
                }
                if sparse_index.len() == sparse_index.capacity() {
                    sparse_index.try_reserve(1).map_err(scan_reserve_failed)?;
                }
                sparse_index.push(SstSparseIndexEntry {
                    key: clone_scan_bytes(&current)?,
                    index_offset: index_entry_offset,
                    ordinal: ordinal as usize,
                    record_offset,
                });
            }
            next_sparse_offset = index_entry_offset.saturating_add(sparse_interval);
        }
        std::mem::swap(&mut previous, &mut current);
        last_offset = Some(record_offset);
        offset = key_end;
    }
    if offset != end {
        return Err(CalyxError::aster_corrupt_shard("SST index length mismatch"));
    }
    let bloom_len_u64 = file_len.checked_sub(header.bloom_offset).ok_or_else(|| {
        CalyxError::aster_corrupt_shard("SST bloom offset exceeds the physical file length")
    })?;
    let bloom_len = usize::try_from(bloom_len_u64)
        .map_err(|_| CalyxError::aster_corrupt_shard("SST bloom section exceeds usize"))?;
    let mut bloom_bytes = Vec::new();
    bloom_bytes
        .try_reserve_exact(bloom_len)
        .map_err(|error| CalyxError {
            code: "CALYX_ASTER_SST_BLOOM_ALLOC",
            message: format!(
                "could not reserve {bloom_len} bytes to validate SST bloom section {}: {error}",
                path.display()
            ),
            remediation: "free host memory or repair the oversized/corrupt SST; the vault refuses to open instead of aborting the daemon",
        })?;
    bloom_bytes.resize(bloom_len, 0);
    read_exact_sst_stream(
        &mut reader,
        &mut bloom_bytes,
        path,
        header.bloom_offset,
        "bloom filter",
    )?;
    let bloom = BloomFilter::decode(&bloom_bytes)?;
    let Some((first_key, first_offset)) = first else {
        if last_offset.is_some() || !previous.is_empty() {
            return Err(CalyxError::aster_corrupt_shard(
                "empty SST bounds walk produced a last key",
            ));
        }
        return Ok(None);
    };
    let Some(last_offset) = last_offset else {
        return Err(CalyxError::aster_corrupt_shard(
            "non-empty SST bounds walk produced no last key",
        ));
    };
    let last_key = previous;
    let mut point_reader = SstPointReader::open(path)?;
    point_reader.validate_record(first_offset, &first_key)?;
    if last_offset != first_offset {
        point_reader.validate_record(last_offset, &last_key)?;
    }
    Ok(Some(SstBounds {
        first_key,
        last_key,
        sparse_index,
        bloom: Arc::new(bloom),
    }))
}

/// Fixed file buffer used while deriving one cold SST's bounds. Router load is
/// sequential, so startup retains exactly one such buffer regardless of vault
/// size or immutable-file count.
const SST_BOUNDS_BUFFER_BYTES: usize = 64 * 1_024;

/// Cold-index seek points are spaced by at least 256 KiB and capped per SST.
/// A normal 64 MiB SST therefore retains at most 258 tiny anchors even if its
/// data consists of millions of small rows, while a malformed/oversized SST
/// can never contribute more than this absolute per-file count.
const SST_SPARSE_INDEX_INTERVAL_BYTES: u64 = 256 * 1_024;
const MAX_SST_SPARSE_INDEX_ENTRIES: usize = 1_024;

fn read_exact_sst_stream(
    reader: &mut impl Read,
    out: &mut [u8],
    path: &Path,
    offset: u64,
    section: &'static str,
) -> Result<()> {
    reader.read_exact(out).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            CalyxError::aster_corrupt_shard(format!(
                "SST {section} is truncated at {}:{offset} while reading {} bytes",
                path.display(),
                out.len()
            ))
        } else {
            sst_io_error(&format!("read SST {section}"), path, error)
        }
    })
}

pub(super) fn materialized_entry_bytes<T>(key: &[u8], value: &[u8]) -> usize {
    std::mem::size_of::<T>()
        .saturating_add(key.len())
        .saturating_add(value.len())
}

pub(super) fn clone_scan_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(scan_reserve_failed)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

struct SstRecordRef<'a> {
    key: &'a [u8],
    value: Cow<'a, [u8]>,
}

#[derive(Debug, Clone, Copy)]
struct Header {
    version: u32,
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

fn write_record(out: &mut Vec<u8>, key: &[u8], value: &[u8], version: u32) -> Result<()> {
    let key_len = u32::try_from(key.len())
        .map_err(|_| CalyxError::disk_pressure("SST key length exceeds u32"))?;
    let value_len = u32::try_from(value.len())
        .map_err(|_| CalyxError::disk_pressure("SST value length exceeds u32"))?;
    let crc = record_crc(key, value);
    let compressed = if version >= VERSION && value.len() >= MIN_COMPRESSION_INPUT_BYTES {
        let candidate = zstd::bulk::compress(value, SST_ZSTD_LEVEL).map_err(|error| CalyxError {
            code: "CALYX_ASTER_SST_COMPRESSION_FAILED",
            message: format!("Zstandard failed while encoding an SST value: {error}"),
            remediation: "preserve the memtable and inspect the exact compression failure; no SST was published",
        })?;
        (candidate
            .len()
            .saturating_add(MIN_COMPRESSION_SAVINGS_BYTES)
            < value.len())
        .then_some(candidate)
    } else {
        None
    };
    let stored = compressed.as_deref().unwrap_or(value);
    let stored_len = u32::try_from(stored.len())
        .map_err(|_| CalyxError::disk_pressure("compressed SST value length exceeds u32"))?;
    out.extend_from_slice(&key_len.to_le_bytes());
    out.extend_from_slice(&stored_len.to_le_bytes());
    if version >= VERSION {
        out.extend_from_slice(&value_len.to_le_bytes());
    }
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(stored);
    Ok(())
}

fn read_record(bytes: &[u8], offset: u64, version: u32) -> Result<SstEntry> {
    let record = read_record_ref(bytes, offset, version)?;
    Ok(SstEntry {
        key: record.key.to_vec(),
        value: record.value.into_owned(),
    })
}

fn read_record_ref(bytes: &[u8], offset: u64, version: u32) -> Result<SstRecordRef<'_>> {
    let offset = offset as usize;
    let record_header_len = if version >= VERSION {
        COMPRESSED_RECORD_HEADER_LEN
    } else {
        RECORD_HEADER_LEN
    };
    let header_end = offset
        .checked_add(record_header_len)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST record header offset overflow"))?;
    let header = bytes
        .get(offset..header_end)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST record header out of bounds"))?;
    let key_len = u32::from_le_bytes(header[0..4].try_into().expect("key len")) as usize;
    let stored_value_len =
        u32::from_le_bytes(header[4..8].try_into().expect("stored value len")) as usize;
    let (value_len, expected_crc) = if version >= VERSION {
        (
            u32::from_le_bytes(header[8..12].try_into().expect("value len")) as usize,
            u32::from_le_bytes(header[12..16].try_into().expect("record crc")),
        )
    } else {
        (
            stored_value_len,
            u32::from_le_bytes(header[8..12].try_into().expect("record crc")),
        )
    };
    if value_len > MAX_RANGE_SCAN_BYTES {
        return Err(scan_budget_exceeded(value_len, value_len));
    }
    let key_start = header_end;
    let value_start = key_start
        .checked_add(key_len)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST key length overflow"))?;
    let value_end = value_start
        .checked_add(stored_value_len)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST value length overflow"))?;
    let key = bytes
        .get(key_start..value_start)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST key out of bounds"))?;
    let stored_value = bytes
        .get(value_start..value_end)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST value out of bounds"))?;
    let value = decode_record_value(stored_value, value_len)?;
    let actual_crc = record_crc(key, value.as_ref());
    if actual_crc != expected_crc {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "SST record crc mismatch at {offset}: expected {expected_crc:08x}, got {actual_crc:08x}"
        )));
    }
    Ok(SstRecordRef { key, value })
}

fn decode_record_value(stored: &[u8], decoded_len: usize) -> Result<Cow<'_, [u8]>> {
    if stored.len() == decoded_len {
        return Ok(Cow::Borrowed(stored));
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(decoded_len)
        .map_err(scan_reserve_failed)?;
    decoded.resize(decoded_len, 0);
    let mut decompressor = zstd::bulk::Decompressor::new().map_err(sst_decompression_error)?;
    let written = decompressor
        .decompress_to_buffer(stored, decoded.as_mut_slice())
        .map_err(sst_decompression_error)?;
    if written != decoded_len {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "SST Zstandard value decoded to {written} bytes, expected {decoded_len}"
        )));
    }
    Ok(Cow::Owned(decoded))
}

fn sst_decompression_error(error: std::io::Error) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!("SST Zstandard value decode failed: {error}"))
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
    version: u32,
) {
    bytes[0..4].copy_from_slice(MAGIC);
    bytes[4..8].copy_from_slice(&version.to_le_bytes());
    bytes[8..12].copy_from_slice(&entries.to_le_bytes());
    bytes[12..20].copy_from_slice(&index_offset.to_le_bytes());
    bytes[20..28].copy_from_slice(&bloom_offset.to_le_bytes());
    bytes[28..32].copy_from_slice(&body_crc.to_le_bytes());
}

fn parse_header_structure(header: &[u8], len: u64) -> Result<Header> {
    if header.len() != HEADER_LEN {
        return Err(CalyxError::aster_corrupt_shard("SST header missing"));
    }
    if &header[0..4] != MAGIC {
        return Err(CalyxError::aster_corrupt_shard("SST magic mismatch"));
    }
    let version = u32::from_le_bytes(header[4..8].try_into().expect("version"));
    if version != VERSION && version != CHECKSUMMED_VERSION && version != LEGACY_VERSION {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "unsupported SST version {version}"
        )));
    }
    let entries = u32::from_le_bytes(header[8..12].try_into().expect("entries"));
    let index_offset = u64::from_le_bytes(header[12..20].try_into().expect("index offset"));
    let bloom_offset = u64::from_le_bytes(header[20..28].try_into().expect("bloom offset"));
    if index_offset < HEADER_LEN as u64
        || index_offset > len
        || bloom_offset < index_offset
        || bloom_offset > len
    {
        return Err(CalyxError::aster_corrupt_shard(
            "SST header offsets out of bounds",
        ));
    }
    Ok(Header {
        version,
        entries,
        index_offset,
        bloom_offset,
    })
}

fn read_header_structure(bytes: &[u8]) -> Result<Header> {
    let header = bytes
        .get(0..HEADER_LEN)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST header missing"))?;
    let len = bytes.len() as u64;
    parse_header_structure(header, len)
}

fn read_file_header_structure(file: &mut File, path: &Path) -> Result<Header> {
    let len = file
        .metadata()
        .map_err(|error| sst_io_error("stat SST", path, error))?
        .len();
    file.seek(SeekFrom::Start(0))
        .map_err(|error| sst_io_error("seek SST header", path, error))?;
    let mut header = [0_u8; HEADER_LEN];
    file.read_exact(&mut header).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            CalyxError::aster_corrupt_shard(format!(
                "SST header is truncated in {}",
                path.display()
            ))
        } else {
            sst_io_error("read SST header", path, error)
        }
    })?;
    parse_header_structure(&header, len)
}

fn sst_io_error(context: &str, path: &Path, error: std::io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!("{context} {}: {error}", path.display()))
}

fn read_header(bytes: &[u8]) -> Result<Header> {
    let parsed = read_header_structure(bytes)?;
    let header = bytes
        .get(0..HEADER_LEN)
        .ok_or_else(|| CalyxError::aster_corrupt_shard("SST header missing"))?;
    let version = u32::from_le_bytes(header[4..8].try_into().expect("version"));
    if version >= CHECKSUMMED_VERSION {
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
    Ok(parsed)
}
