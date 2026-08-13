use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use calyx_core::{CalyxError, Result};

use super::{
    HEADER_LEN, INDEX_ENTRY_FIXED_LEN, IndexEntry, MAX_RANGE_SCAN_BYTES, RECORD_HEADER_LEN,
    SstBounds, SstEntry, SstLookupMetadata, clone_scan_bytes, materialized_entry_bytes,
    read_file_header_structure, record_crc, scan_reserve_failed,
};

/// Uses an already whole-file-validated immutable SST index and performs
/// bounded row reads with one retained file handle.
#[derive(Debug)]
pub(crate) struct SstStreamingReader {
    path: PathBuf,
    index: Vec<IndexEntry>,
    point_reader: SstPointReader,
}

impl SstStreamingReader {
    /// Opens a one-pass streaming cursor after validating the immutable SST.
    ///
    /// Candidate paging uses [`SstPageReader`] instead so it never repeats the
    /// whole-file validation pass or clones a retained index. This constructor
    /// remains for compaction, whose one-pass input open is the boundary.
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        // Compaction owns this decoded index for the duration of its stream.
        // Caching the source reader retained a second index plus the complete
        // mmap after this clone, long after compaction stopped reading it.
        let index = super::SstReader::open(path)?.validated_index();
        let path = path.to_path_buf();
        let point_reader = SstPointReader::open(&path)?;
        Ok(Self {
            path,
            index,
            point_reader,
        })
    }

    pub(crate) fn key_at(&self, position: usize) -> Option<&[u8]> {
        self.index.get(position).map(|entry| entry.key.as_slice())
    }

    pub(crate) fn entry_at(&mut self, position: usize) -> Result<SstEntry> {
        let indexed = self.index.get(position).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "SST streaming row position {position} is outside index length {} in {}",
                self.index.len(),
                self.path.display()
            ))
        })?;
        let value = self.point_reader.read_value(indexed.offset, &indexed.key)?;
        Ok(SstEntry {
            key: indexed.key.clone(),
            value,
        })
    }
}

/// Candidate-page reader over either a retained lookup or the immutable on-disk
/// index.
///
/// Cold files stream their variable-length index through a bounded buffered file
/// reader: the cursor retains one current key, never a `Vec` per key or a mapping
/// of the complete SST. This keeps paging available for every column family
/// without recreating the multi-gigabyte decoded-index resident set from #2239
/// or charging faulted cold pages to the daemon's working set. Each index entry
/// is checked against the CRC-validated record it selects before that row can be
/// served. Validation is deliberately incremental: checksumming every byte in
/// every intersecting 64 MiB SST at cursor open faults the complete cold corpus
/// into memory before a row is requested.
#[derive(Debug)]
pub(crate) enum SstPageReader {
    Retained {
        path: PathBuf,
        lookup: Arc<SstLookupMetadata>,
        point_reader: SstPointReader,
        position: usize,
    },
    Streaming(DiskIndexCursor),
}

impl SstPageReader {
    pub(crate) fn open(path: PathBuf, lookup: Arc<SstLookupMetadata>) -> Result<Self> {
        let point_reader = SstPointReader::open(&path)?;
        Ok(Self::Retained {
            path,
            lookup,
            point_reader,
            position: 0,
        })
    }

    pub(crate) fn open_streaming(path: PathBuf, bounds: Arc<SstBounds>) -> Result<Self> {
        DiskIndexCursor::open(path, bounds).map(SstPageReader::Streaming)
    }

    pub(crate) fn seek_lower_bound(&mut self, key: &[u8], exclusive: bool) -> Result<()> {
        match self {
            Self::Retained {
                lookup, position, ..
            } => {
                *position = lookup.lower_bound(key, exclusive);
                Ok(())
            }
            Self::Streaming(reader) => reader.seek_lower_bound(key, exclusive),
        }
    }

    pub(crate) fn current_key(&self) -> Option<&[u8]> {
        match self {
            Self::Retained {
                lookup, position, ..
            } => lookup.key_at(*position),
            Self::Streaming(reader) => reader.current_key(),
        }
    }

    pub(crate) fn read_current(&mut self) -> Result<SstEntry> {
        match self {
            Self::Retained {
                path,
                lookup,
                point_reader,
                position,
            } => {
                let (key, offset) = lookup.entry_at(*position).ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "SST page row position {position} is outside retained index length {} in {}",
                        lookup.len(),
                        path.display()
                    ))
                })?;
                let value = point_reader.read_value(offset, key)?;
                Ok(SstEntry {
                    key: key.to_vec(),
                    value,
                })
            }
            Self::Streaming(reader) => reader.read_current(),
        }
    }

    pub(crate) fn advance_one(&mut self) -> Result<()> {
        match self {
            Self::Retained { position, .. } => {
                *position = position.saturating_add(1);
                Ok(())
            }
            Self::Streaming(reader) => reader.advance_one(),
        }
    }

    pub(crate) fn advance_past(&mut self, key: &[u8]) -> Result<()> {
        while self.current_key().is_some_and(|candidate| candidate == key) {
            self.advance_one()?;
        }
        Ok(())
    }

    pub(crate) fn predecessor(
        &mut self,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Result<Option<SstEntry>> {
        match self {
            Self::Retained {
                lookup, position, ..
            } => {
                let end = lookup.lower_bound(upper, inclusive);
                let Some(candidate) = end.checked_sub(1) else {
                    return Ok(None);
                };
                *position = candidate;
                if self.current_key().is_none_or(|key| key < start) {
                    return Ok(None);
                }
                self.read_current().map(Some)
            }
            Self::Streaming(reader) => reader.predecessor(start, upper, inclusive),
        }
    }
}

#[derive(Debug, Clone)]
struct DiskIndexEntry {
    key: Vec<u8>,
    record_offset: u64,
}

/// Allocation-constant cursor over the variable-length on-disk SST index.
#[derive(Debug)]
pub(crate) struct DiskIndexCursor {
    index_reader: BufReader<File>,
    point_reader: SstPointReader,
    path: PathBuf,
    data_end: u64,
    index_end: u64,
    entries: usize,
    ordinal: usize,
    next_index_offset: u64,
    current: Option<DiskIndexEntry>,
    bounds: Arc<SstBounds>,
}

impl DiskIndexCursor {
    fn open(path: PathBuf, bounds: Arc<SstBounds>) -> Result<Self> {
        let result = (|| {
            let point_reader = SstPointReader::open(&path)?;
            let data_end = point_reader.data_end;
            let index_end = point_reader.index_end;
            let entries = point_reader.entries;
            let mut index_file = OpenOptions::new()
                .read(true)
                .open(&path)
                .map_err(|error| storage_error("open SST streaming index", &path, error))?;
            #[cfg(target_os = "linux")]
            {
                use nix::fcntl::{PosixFadviseAdvice, posix_fadvise};

                posix_fadvise(
                    &index_file,
                    data_end as i64,
                    index_end.saturating_sub(data_end) as i64,
                    PosixFadviseAdvice::POSIX_FADV_SEQUENTIAL,
                )
                .map_err(|error| {
                    storage_error(
                        "declare sequential SST index access",
                        &path,
                        io::Error::from(error),
                    )
                })?;
                posix_fadvise(
                    &index_file,
                    data_end as i64,
                    index_end.saturating_sub(data_end) as i64,
                    PosixFadviseAdvice::POSIX_FADV_NOREUSE,
                )
                .map_err(|error| {
                    storage_error(
                        "declare one-pass SST index access",
                        &path,
                        io::Error::from(error),
                    )
                })?;
            }
            index_file
                .seek(SeekFrom::Start(data_end))
                .map_err(|error| storage_error("seek SST streaming index", &path, error))?;
            let mut cursor = Self {
                index_reader: BufReader::with_capacity(STREAMING_INDEX_BUFFER_BYTES, index_file),
                point_reader,
                path: path.clone(),
                data_end,
                index_end,
                entries,
                ordinal: 0,
                next_index_offset: data_end,
                current: None,
                bounds,
            };
            if entries == 0 {
                if data_end != index_end {
                    return Err(CalyxError::aster_corrupt_shard(
                        "empty SST index has non-empty bytes",
                    ));
                }
            } else {
                cursor.current = Some(cursor.parse_next_entry(0)?);
            }
            Ok(cursor)
        })();
        result.map_err(|mut error| {
            error.message = format!(
                "open streaming SST page index {}: {}",
                path.display(),
                error.message
            );
            error
        })
    }

    fn parse_next_entry(&mut self, ordinal: usize) -> Result<DiskIndexEntry> {
        let offset = self.next_index_offset;
        let fixed_end = offset
            .checked_add(INDEX_ENTRY_FIXED_LEN as u64)
            .ok_or_else(|| {
                CalyxError::aster_corrupt_shard("SST streaming index fixed offset overflow")
            })?;
        if fixed_end > self.index_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST streaming index entry {ordinal} at {offset} is out of bounds"
            )));
        }
        let mut fixed = [0_u8; INDEX_ENTRY_FIXED_LEN];
        read_exact_indexed(&mut self.index_reader, &mut fixed, &self.path, offset)?;
        let key_len = u32::from_le_bytes(fixed[0..4].try_into().expect("index key len")) as usize;
        let record_offset = u64::from_le_bytes(fixed[4..12].try_into().expect("record offset"));
        let key_len_u64 = u64::try_from(key_len)
            .map_err(|_| CalyxError::aster_corrupt_shard("SST index key length exceeds u64"))?;
        let key_end = fixed_end.checked_add(key_len_u64).ok_or_else(|| {
            CalyxError::aster_corrupt_shard("SST streaming index key offset overflow")
        })?;
        if key_end > self.index_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST streaming index key {ordinal} ending at {key_end} is out of bounds"
            )));
        }
        let mut key = try_zeroed(key_len)?;
        read_exact_indexed(&mut self.index_reader, &mut key, &self.path, fixed_end)?;
        if record_offset < HEADER_LEN as u64 || record_offset >= self.data_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST streaming index record offset {record_offset} is outside the data section {}..{}",
                HEADER_LEN, self.data_end
            )));
        }
        if ordinal + 1 == self.entries && key_end != self.index_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST streaming index length mismatch after final entry {ordinal}: next_offset={key_end} index_end={}",
                self.index_end
            )));
        }
        self.next_index_offset = key_end;
        Ok(DiskIndexEntry { key, record_offset })
    }

    fn current_key(&self) -> Option<&[u8]> {
        self.current.as_ref().map(|current| current.key.as_slice())
    }

    fn seek_lower_bound(&mut self, key: &[u8], exclusive: bool) -> Result<()> {
        self.seek_near(key, true)?;
        while self.current_key().is_some_and(|candidate| {
            if exclusive {
                candidate <= key
            } else {
                candidate < key
            }
        }) {
            self.advance_one()?;
        }
        Ok(())
    }

    fn predecessor(
        &mut self,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Result<Option<SstEntry>> {
        self.seek_near(upper, inclusive)?;
        let mut candidate = None;
        while let Some(current) = self.current.as_ref() {
            if current.key.as_slice() > upper || (!inclusive && current.key.as_slice() == upper) {
                break;
            }
            if current.key.as_slice() >= start {
                candidate = Some(current.clone());
            }
            self.advance_one()?;
        }
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let value = self
            .point_reader
            .read_value(candidate.record_offset, &candidate.key)?;
        Ok(Some(SstEntry {
            key: candidate.key,
            value,
        }))
    }

    /// Repositions at the greatest retained seek point that cannot skip a
    /// possible answer. The subsequent linear walk is bounded by one sparse
    /// interval and still validates the selected record before serving it.
    fn seek_near(&mut self, key: &[u8], inclusive: bool) -> Result<()> {
        let anchor_end = self.bounds.sparse_index.partition_point(|anchor| {
            if inclusive {
                anchor.key.as_slice() <= key
            } else {
                anchor.key.as_slice() < key
            }
        });
        let Some(anchor) = anchor_end
            .checked_sub(1)
            .and_then(|position| self.bounds.sparse_index.get(position))
            .cloned()
        else {
            return Ok(());
        };
        if anchor.ordinal <= self.ordinal {
            return Ok(());
        }
        self.index_reader
            .seek(SeekFrom::Start(anchor.index_offset))
            .map_err(|error| storage_error("seek sparse SST streaming index", &self.path, error))?;
        self.ordinal = anchor.ordinal;
        self.next_index_offset = anchor.index_offset;
        let current = self.parse_next_entry(anchor.ordinal)?;
        if current.key != anchor.key || current.record_offset != anchor.record_offset {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST sparse index anchor drift at ordinal {} in {}",
                anchor.ordinal,
                self.path.display()
            )));
        }
        self.current = Some(current);
        Ok(())
    }

    fn read_current(&mut self) -> Result<SstEntry> {
        let current = self.current.as_ref().ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "SST streaming page cursor is exhausted in {}",
                self.path.display()
            ))
        })?;
        let indexed_key = current.key.as_slice();
        let value = self
            .point_reader
            .read_value(current.record_offset, indexed_key)?;
        let row_bytes = materialized_entry_bytes::<SstEntry>(indexed_key, &value);
        if row_bytes > MAX_RANGE_SCAN_BYTES {
            return Err(CalyxError {
                code: "CALYX_ASTER_SCAN_MEMORY_BUDGET",
                message: format!(
                    "SST streaming page row at ordinal {} in {} requires {row_bytes} bytes, above the {MAX_RANGE_SCAN_BYTES}-byte materialization budget",
                    self.ordinal,
                    self.path.display()
                ),
                remediation: "repair or split the oversized immutable row; paging refuses an allocation that can abort the daemon",
            });
        }
        Ok(SstEntry {
            key: clone_scan_bytes(indexed_key)?,
            value,
        })
    }

    fn advance_one(&mut self) -> Result<()> {
        let Some(current) = self.current.take() else {
            return Ok(());
        };
        let next_ordinal = self.ordinal.saturating_add(1);
        if next_ordinal >= self.entries {
            if self.next_index_offset != self.index_end {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "SST streaming index ended at {} instead of {} in {}",
                    self.next_index_offset,
                    self.index_end,
                    self.path.display()
                )));
            }
            self.ordinal = self.entries;
            self.current = None;
            return Ok(());
        }
        let next = self.parse_next_entry(next_ordinal)?;
        if current.key >= next.key {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST streaming index keys are not strictly sorted at ordinals {} and {next_ordinal} in {}",
                self.ordinal,
                self.path.display()
            )));
        }
        self.ordinal = next_ordinal;
        self.current = Some(next);
        Ok(())
    }
}

/// Buffer retained by each cold SST cursor. Even at the hard 512-source fan-in
/// ceiling this accounts for at most 8 MiB of index buffers per page operation.
const STREAMING_INDEX_BUFFER_BYTES: usize = 16 * 1_024;

/// Data buffer retained by each SST point reader. Page scans visit record
/// offsets in physical order, so retaining the next 64 KiB turns thousands of
/// tiny row reads into bounded sequential I/O. At the hard 512-source fan-in
/// ceiling this accounts for at most 32 MiB of data buffers per page operation.
const STREAMING_DATA_BUFFER_BYTES: usize = 64 * 1_024;

#[derive(Debug)]
pub(crate) struct SstPointReader {
    file: BufReader<File>,
    path: PathBuf,
    cursor_offset: u64,
    data_end: u64,
    index_end: u64,
    entries: usize,
}

#[derive(Clone, Copy, Debug)]
struct IndexedRecordLayout {
    key_len: usize,
    value_len: usize,
    key_start: u64,
    value_start: u64,
    expected_crc: u32,
}

impl SstPointReader {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|error| storage_error("open SST for indexed row read", path, error))?;
        #[cfg(target_os = "linux")]
        {
            use nix::fcntl::{PosixFadviseAdvice, posix_fadvise};

            posix_fadvise(&file, 0, 0, PosixFadviseAdvice::POSIX_FADV_RANDOM).map_err(|error| {
                storage_error(
                    "declare random SST point-read access",
                    path,
                    io::Error::from(error),
                )
            })?;
            posix_fadvise(&file, 0, 0, PosixFadviseAdvice::POSIX_FADV_NOREUSE).map_err(
                |error| {
                    storage_error(
                        "declare one-pass SST point-read access",
                        path,
                        io::Error::from(error),
                    )
                },
            )?;
        }
        let header = read_file_header_structure(&mut file, path)?;
        let entries = usize::try_from(header.entries)
            .map_err(|_| CalyxError::aster_corrupt_shard("SST entry count exceeds usize"))?;
        Ok(Self {
            file: BufReader::with_capacity(STREAMING_DATA_BUFFER_BYTES, file),
            path: path.to_path_buf(),
            cursor_offset: HEADER_LEN as u64,
            data_end: header.index_offset,
            index_end: header.bloom_offset,
            entries,
        })
    }

    /// Reads and CRC-validates one exact SST record after matching its index key.
    pub(crate) fn read_value(
        &mut self,
        record_offset: u64,
        expected_key: &[u8],
    ) -> Result<Vec<u8>> {
        let layout = self.read_record_layout(record_offset, expected_key)?;
        let row_bytes = std::mem::size_of::<SstEntry>()
            .saturating_add(layout.key_len)
            .saturating_add(layout.value_len);
        if row_bytes > MAX_RANGE_SCAN_BYTES {
            return Err(CalyxError {
                code: "CALYX_ASTER_SCAN_MEMORY_BUDGET",
                message: format!(
                    "SST indexed record at {}:{record_offset} requires {row_bytes} bytes, above the {MAX_RANGE_SCAN_BYTES}-byte materialization budget",
                    self.path.display()
                ),
                remediation: "repair or split the oversized immutable row; indexed reads refuse an allocation that can abort the daemon",
            });
        }
        let mut key = try_zeroed(layout.key_len)?;
        self.read_exact_at(&mut key, layout.key_start)?;
        if key != expected_key {
            return Err(index_key_mismatch(
                &self.path,
                record_offset,
                &key,
                expected_key,
            ));
        }
        let mut value = try_zeroed(layout.value_len)?;
        self.read_exact_at(&mut value, layout.value_start)?;
        let actual_crc = record_crc(&key, &value);
        if actual_crc != layout.expected_crc {
            return Err(record_crc_mismatch(
                &self.path,
                record_offset,
                layout.expected_crc,
                actual_crc,
            ));
        }
        Ok(value)
    }

    /// Validates one record without materializing its value.
    ///
    /// Cold-open bounds need integrity proof for the first and last keys of
    /// every SST, but retaining either value would turn a metadata census into
    /// a data-sized allocation. CRC input is therefore consumed through one
    /// fixed buffer.
    pub(crate) fn validate_record(
        &mut self,
        record_offset: u64,
        expected_key: &[u8],
    ) -> Result<()> {
        let layout = self.read_record_layout(record_offset, expected_key)?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buffer = [0_u8; RECORD_VALIDATION_BUFFER_BYTES];
        let mut cursor = layout.key_start;
        for expected in expected_key.chunks(RECORD_VALIDATION_BUFFER_BYTES) {
            let actual = &mut buffer[..expected.len()];
            self.read_exact_at(actual, cursor)?;
            if actual != expected {
                return Err(index_key_mismatch(
                    &self.path,
                    record_offset,
                    actual,
                    expected,
                ));
            }
            hasher.update(actual);
            cursor = cursor.saturating_add(expected.len() as u64);
        }
        let mut remaining = layout.value_len;
        cursor = layout.value_start;
        while remaining > 0 {
            let chunk_len = remaining.min(buffer.len());
            let chunk = &mut buffer[..chunk_len];
            self.read_exact_at(chunk, cursor)?;
            hasher.update(chunk);
            remaining -= chunk_len;
            cursor = cursor.saturating_add(chunk_len as u64);
        }
        let actual_crc = hasher.finalize();
        if actual_crc != layout.expected_crc {
            return Err(record_crc_mismatch(
                &self.path,
                record_offset,
                layout.expected_crc,
                actual_crc,
            ));
        }
        Ok(())
    }

    fn read_record_layout(
        &mut self,
        record_offset: u64,
        expected_key: &[u8],
    ) -> Result<IndexedRecordLayout> {
        if record_offset < HEADER_LEN as u64 || record_offset >= self.data_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST indexed record offset {record_offset} is outside data section {}..{} in {}",
                HEADER_LEN,
                self.data_end,
                self.path.display()
            )));
        }
        let mut header = [0_u8; RECORD_HEADER_LEN];
        self.read_exact_at(&mut header, record_offset)?;
        let key_len = u32::from_le_bytes(header[0..4].try_into().expect("key len")) as usize;
        let value_len = u32::from_le_bytes(header[4..8].try_into().expect("value len")) as usize;
        let expected_crc = u32::from_le_bytes(header[8..12].try_into().expect("record crc"));
        let key_start = record_offset
            .checked_add(RECORD_HEADER_LEN as u64)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("SST key offset overflow"))?;
        let value_start = key_start
            .checked_add(key_len as u64)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("SST value offset overflow"))?;
        let value_end = value_start
            .checked_add(value_len as u64)
            .ok_or_else(|| CalyxError::aster_corrupt_shard("SST value length overflow"))?;
        if value_end > self.data_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST indexed record {record_offset} ends at {value_end}, beyond data section end {} in {}",
                self.data_end,
                self.path.display()
            )));
        }
        if key_len != expected_key.len() {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST indexed record at {}:{record_offset} declares a {key_len}-byte key but its index key is {} bytes",
                self.path.display(),
                expected_key.len()
            )));
        }
        Ok(IndexedRecordLayout {
            key_len,
            value_len,
            key_start,
            value_start,
            expected_crc,
        })
    }

    /// Reads at an absolute SST offset without issuing a seek when the next
    /// record begins exactly where the previous read ended. This invariant is
    /// what makes ordered page scans sequential while preserving random point
    /// reads and their exact corruption diagnostics.
    fn read_exact_at(&mut self, out: &mut [u8], offset: u64) -> Result<()> {
        if self.cursor_offset != offset {
            self.file
                .seek(SeekFrom::Start(offset))
                .map_err(|error| storage_error("seek SST indexed row", &self.path, error))?;
            self.cursor_offset = offset;
        }
        read_exact_indexed(&mut self.file, out, &self.path, offset)?;
        self.cursor_offset = offset.checked_add(out.len() as u64).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "SST indexed row cursor overflow at {}:{offset} while reading {} bytes",
                self.path.display(),
                out.len()
            ))
        })?;
        Ok(())
    }
}

const RECORD_VALIDATION_BUFFER_BYTES: usize = 16 * 1_024;

fn index_key_mismatch(
    path: &Path,
    record_offset: u64,
    actual: &[u8],
    expected: &[u8],
) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!(
        "SST indexed record at {}:{record_offset} has key {} instead of {}",
        path.display(),
        hex_bytes(actual),
        hex_bytes(expected)
    ))
}

fn record_crc_mismatch(
    path: &Path,
    record_offset: u64,
    expected_crc: u32,
    actual_crc: u32,
) -> CalyxError {
    CalyxError::aster_corrupt_shard(format!(
        "SST indexed record CRC mismatch at {}:{record_offset}: expected {expected_crc:08x}, got {actual_crc:08x}",
        path.display()
    ))
}

fn try_zeroed(len: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(scan_reserve_failed)?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn read_exact_indexed(
    reader: &mut impl Read,
    out: &mut [u8],
    path: &Path,
    offset: u64,
) -> Result<()> {
    reader.read_exact(out).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            CalyxError::aster_corrupt_shard(format!(
                "SST indexed row is truncated at {}:{offset} while reading {} bytes",
                path.display(),
                out.len()
            ))
        } else {
            storage_error("read SST indexed row", path, error)
        }
    })
}

fn storage_error(context: &str, path: &Path, error: io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!("{context} {}: {error}", path.display()))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
