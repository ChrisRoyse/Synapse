use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, Result};

use super::{
    HEADER_LEN, IndexEntry, LEGACY_VERSION, MAGIC, RECORD_HEADER_LEN, SstEntry, SstLookupMetadata,
    VERSION, record_crc,
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
        let index = super::shared_reader(path)?.validated_index();
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

/// Candidate-page reader borrowing a retained, whole-file-validated lookup.
///
/// Opening performs only bounded header/file-handle work. It never clones the
/// full index and never rechecks the full SST body.
#[derive(Debug)]
pub(crate) struct SstPageReader<'a> {
    path: &'a Path,
    lookup: &'a SstLookupMetadata,
    point_reader: SstPointReader,
}

impl<'a> SstPageReader<'a> {
    pub(crate) fn open(path: &'a Path, lookup: &'a SstLookupMetadata) -> Result<Self> {
        Ok(Self {
            path,
            lookup,
            point_reader: SstPointReader::open(path)?,
        })
    }

    pub(crate) fn lower_bound(&self, key: &[u8], exclusive: bool) -> usize {
        self.lookup.lower_bound(key, exclusive)
    }

    pub(crate) fn key_at(&self, position: usize) -> Option<&[u8]> {
        self.lookup.key_at(position)
    }

    pub(crate) fn entry_at(&mut self, position: usize) -> Result<SstEntry> {
        let (key, offset) = self.lookup.entry_at(position).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "SST page row position {position} is outside retained index length {} in {}",
                self.lookup.len(),
                self.path.display()
            ))
        })?;
        let value = self.point_reader.read_value(offset, key)?;
        Ok(SstEntry {
            key: key.to_vec(),
            value,
        })
    }
}

#[derive(Debug)]
pub(crate) struct SstPointReader {
    file: File,
    path: PathBuf,
    data_end: u64,
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
        let file_len = file
            .metadata()
            .map_err(|error| storage_error("stat SST for indexed row read", path, error))?
            .len();
        let mut header = [0_u8; HEADER_LEN];
        read_exact_indexed(&mut file, &mut header, path, 0)?;
        if &header[0..4] != MAGIC {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST {} magic mismatch during indexed row read",
                path.display()
            )));
        }
        let version = u32::from_le_bytes(header[4..8].try_into().expect("version"));
        if version != VERSION && version != LEGACY_VERSION {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "unsupported SST version {version} in {}",
                path.display()
            )));
        }
        let index_offset = u64::from_le_bytes(header[12..20].try_into().expect("index offset"));
        let bloom_offset = u64::from_le_bytes(header[20..28].try_into().expect("bloom offset"));
        if index_offset < HEADER_LEN as u64
            || index_offset > file_len
            || bloom_offset < index_offset
            || bloom_offset > file_len
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST {} header offsets are out of bounds for file length {file_len}",
                path.display()
            )));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            data_end: index_offset,
        })
    }

    /// Reads and CRC-validates one exact SST record. The caller separately
    /// authenticates the value against the SHA-256 stored in the page index.
    pub(crate) fn read_value(
        &mut self,
        record_offset: u64,
        expected_key: &[u8],
    ) -> Result<Vec<u8>> {
        if record_offset < HEADER_LEN as u64 || record_offset >= self.data_end {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST indexed record offset {record_offset} is outside data section {}..{} in {}",
                HEADER_LEN,
                self.data_end,
                self.path.display()
            )));
        }
        self.file
            .seek(SeekFrom::Start(record_offset))
            .map_err(|error| storage_error("seek SST indexed row", &self.path, error))?;
        let mut header = [0_u8; RECORD_HEADER_LEN];
        read_exact_indexed(&mut self.file, &mut header, &self.path, record_offset)?;
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
        let mut key = vec![0_u8; key_len];
        read_exact_indexed(&mut self.file, &mut key, &self.path, key_start)?;
        if key != expected_key {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST indexed record at {}:{record_offset} has key {} instead of {}",
                self.path.display(),
                hex_bytes(&key),
                hex_bytes(expected_key)
            )));
        }
        let mut value = vec![0_u8; value_len];
        read_exact_indexed(&mut self.file, &mut value, &self.path, value_start)?;
        let actual_crc = record_crc(&key, &value);
        if actual_crc != expected_crc {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "SST indexed record CRC mismatch at {}:{record_offset}: expected {expected_crc:08x}, got {actual_crc:08x}",
                self.path.display()
            )));
        }
        Ok(value)
    }
}

fn read_exact_indexed(file: &mut File, out: &mut [u8], path: &Path, offset: u64) -> Result<()> {
    file.read_exact(out).map_err(|error| {
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
