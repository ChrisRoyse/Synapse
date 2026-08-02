//! Fixed-size, pre-allocated, in-place publication of derived Ledger
//! projections (#1947).
//!
//! # Why this exists
//!
//! The two derived Ledger projections (`ledger_head/current.json` and
//! `ledger_head/latest_checkpoint.json`) were published with the
//! create-temp -> write -> rename sequence that is the correct default for a
//! file that is its own authority. #1946 established that these two are *not*
//! their own authority — they are recomputed from durable Ledger rows during
//! recovery — and removed their durability barriers. That left the namespace
//! metadata, which #1947 measured as the residual cost:
//!
//! ```text
//! publish method                          quiet volume   after a 256 KB fsync'd write
//! A  create+rename (the previous shape)      1.344 ms            0.956 ms
//! B  in-place write, no fsync                0.130 ms            0.208 ms
//! C  in-place write + fsync                  1.004 ms            1.260 ms
//! ```
//!
//! Both the create and the rename mutate parent-directory metadata, and on
//! NTFS that is a logged transaction against `$LogFile`. A pre-allocated file
//! that never changes length never touches directory metadata after it is
//! created, so an in-place publish is 6-10x cheaper for the same bytes.
//!
//! # What replaces the atomicity the rename was buying
//!
//! The rename was buying one property: a concurrent reader must never observe
//! a half-written record. Dropping it without replacement would be a
//! correctness regression, so it is replaced deliberately.
//!
//! Every record carries a length and a CRC32 over its own header and payload.
//! A reader that observes a partially-applied write fails the CRC and routes
//! to `CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE` — the same rebuild-from-WAL
//! path #1946 added for a torn projection, reached by the same code, because
//! it is the same condition. The record is one 4 KiB page written at offset 0
//! of a file pre-allocated to exactly that length, which is the largest unit a
//! device is plausibly atomic over and the smallest that never needs a
//! read-modify-write on a 4Kn or 512e device.
//!
//! This is *stronger* than what it replaces, not weaker. `MoveFileEx` with
//! `MOVEFILE_REPLACE_EXISTING` is atomic for the namespace but is not
//! documented to be crash-atomic on NTFS — the unlink of the target and the
//! insertion of the new name need not be committed to `$LogFile` together — so
//! the previous shape could already leave a torn *outcome* after power loss,
//! and had no checksum with which to detect it. A validated record detects
//! every torn state explicitly instead of trusting that it cannot happen. The
//! design follows PostgreSQL's relation-map file, which stores a fixed-size
//! record with a magic number and a CRC and validates both on read.
//!
//! # The file is created once and never unlinked
//!
//! An absent anchor is published as a *vacant* record rather than by deleting
//! the file. That keeps the invariant that no publish after the first ever
//! mutates the namespace, and it removes a hazard the delete-based shape had:
//! runtime Ledger reconciliation can replace these projections while a writer
//! holds the file open, and unlinking a file out from under an open handle is
//! precisely the sharing hazard #1568 catalogued on this platform.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, Result};

/// One record is one 4 KiB page: aligned to the physical sector of every
/// device this runs on, so a publish is a single page write with no
/// read-modify-write, and the file length never changes after creation.
pub(crate) const PROJECTION_RECORD_BYTES: usize = 4096;

/// `magic` (8) + `format_version` (4) + `payload_len` (4) + `crc32` (4).
const HEADER_BYTES: usize = 20;

/// The largest payload one record can carry. Both current payloads are under
/// 200 bytes; a payload that outgrows this fails closed rather than being
/// truncated into a record that would decode as valid.
pub(crate) const MAX_PAYLOAD_BYTES: usize = PROJECTION_RECORD_BYTES - HEADER_BYTES;

const FORMAT_VERSION: u32 = 1;

/// Distinguishes the two projections from each other so a record that is
/// somehow published to the wrong path is rejected rather than decoded as a
/// structurally-similar sibling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionKind {
    LedgerHead,
    LedgerCheckpoint,
}

impl ProjectionKind {
    const fn magic(self) -> &'static [u8; 8] {
        match self {
            Self::LedgerHead => b"CLXLHEAD",
            Self::LedgerCheckpoint => b"CLXLCKPT",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::LedgerHead => "Aster ledger head",
            Self::LedgerCheckpoint => "Aster ledger checkpoint pointer",
        }
    }
}

/// What one record decoded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectionRecord {
    /// A published payload.
    Present(Vec<u8>),
    /// A validly-published *absence*. Semantically identical to the file not
    /// existing, and produced when recovery finds no anchor in the durable
    /// Ledger rows.
    Vacant,
}

/// Per-phase cost of one publish, in microseconds (#1947 ask 2).
///
/// The whole point of #1947 ask 2 is that a 147-byte publish costing 20 ms on
/// large-batch commits was not explainable from outside the process. These
/// splits are the fixed points inside it: an `open` that is non-zero after the
/// first publish means the handle was dropped and reacquired, and a `write`
/// that is large on a pre-allocated in-place page cannot be namespace
/// metadata, because this publish performs none.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PublishTimings {
    pub(crate) open_us: u64,
    pub(crate) write_us: u64,
    pub(crate) sync_us: u64,
    /// True when this publish had to create or re-open the backing file.
    pub(crate) reopened: bool,
}

fn projection_error(label: &str, operation: &str, path: &Path, detail: String) -> CalyxError {
    CalyxError::disk_pressure(format!(
        "{operation} for {label} projection path={}: {detail}",
        path.display()
    ))
}

fn io_detail(error: &io::Error) -> String {
    format!(
        "kind={:?} raw_os_error={:?} error={error}",
        error.kind(),
        error.raw_os_error()
    )
}

/// Encodes one record into its fixed-size page.
///
/// A `None` payload encodes a vacant record. The CRC covers the header bytes
/// that precede it *and* the payload, so a torn write that mixes an old header
/// with a new payload — or the reverse — fails validation just as a torn
/// payload does.
pub(crate) fn encode_record(
    kind: ProjectionKind,
    payload: Option<&[u8]>,
    path: &Path,
) -> Result<[u8; PROJECTION_RECORD_BYTES]> {
    let payload = payload.unwrap_or(&[]);
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(projection_error(
            kind.label(),
            "encode projection record",
            path,
            format!(
                "payload_len={} exceeds the {MAX_PAYLOAD_BYTES}-byte record capacity; \
                 refusing to publish a truncated record",
                payload.len()
            ),
        ));
    }
    let mut record = [0_u8; PROJECTION_RECORD_BYTES];
    record[0..8].copy_from_slice(kind.magic());
    record[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    let payload_len = u32::try_from(payload.len()).map_err(|error| {
        projection_error(
            kind.label(),
            "encode projection record",
            path,
            format!("payload length does not fit a u32: {error}"),
        )
    })?;
    record[12..16].copy_from_slice(&payload_len.to_le_bytes());
    record[HEADER_BYTES..HEADER_BYTES + payload.len()].copy_from_slice(payload);
    let crc = record_crc(&record[0..16], payload);
    record[16..20].copy_from_slice(&crc.to_le_bytes());
    Ok(record)
}

fn record_crc(header_prefix: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header_prefix);
    hasher.update(payload);
    hasher.finalize()
}

/// Every magic this format defines.
const ALL_MAGICS: [&[u8; 8]; 2] = [b"CLXLHEAD", b"CLXLCKPT"];

/// What the first bytes of a projection file say it is.
///
/// The three arms are mutually exclusive and each one names exactly what was
/// observed, which is the point. An earlier version discriminated on *this
/// kind's* magic alone, so a record carrying the sibling projection's magic
/// matched neither and fell into the bare-JSON arm — where it failed as a JSON
/// parse error and was reported to the operator as a legacy-format file. The
/// recovery verdict was still correct (rebuild it), but it was reached through
/// a false description of the evidence, which is worse than either a wrong
/// verdict or a silent one because it sends the reader somewhere real to look
/// for a problem that is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionShape {
    /// Carries one of this format's record magics.
    Record,
    /// The pre-record bare-JSON projection: a JSON object and nothing else.
    LegacyBareJson,
    /// Neither. Reported as-is rather than guessed at.
    Unrecognized,
}

pub(crate) fn classify_projection_bytes(bytes: &[u8]) -> ProjectionShape {
    if bytes.len() >= 8 && ALL_MAGICS.iter().any(|magic| &bytes[0..8] == *magic) {
        return ProjectionShape::Record;
    }
    // The pre-record projection was `serde_json::to_vec` of the anchor struct,
    // so it always begins with `{`. Anything else is neither format and must
    // say so.
    if bytes.first() == Some(&b'{') {
        return ProjectionShape::LegacyBareJson;
    }
    ProjectionShape::Unrecognized
}

/// Decodes one record, returning a structured reason on every rejection.
///
/// Every `Err` here means "this projection must be rebuilt from the durable
/// Ledger rows", never "the Ledger is damaged". The caller maps it to
/// `CALYX_LEDGER_DERIVED_PROJECTION_UNREADABLE`.
pub(crate) fn decode_record(
    kind: ProjectionKind,
    bytes: &[u8],
) -> std::result::Result<ProjectionRecord, String> {
    if bytes.len() != PROJECTION_RECORD_BYTES {
        return Err(format!(
            "record is {} bytes, expected exactly {PROJECTION_RECORD_BYTES}",
            bytes.len()
        ));
    }
    if &bytes[0..8] != kind.magic() {
        return Err(format!(
            "record magic {:02x?} does not match the {:?} projection magic {:02x?}",
            &bytes[0..8],
            kind,
            kind.magic()
        ));
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if version != FORMAT_VERSION {
        return Err(format!(
            "record format version {version} is not the supported version {FORMAT_VERSION}"
        ));
    }
    let payload_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    // Bound-check before slicing: a torn header can carry an arbitrary length,
    // and this must be a rebuild verdict rather than a panic.
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(format!(
            "record payload_len={payload_len} exceeds the {MAX_PAYLOAD_BYTES}-byte capacity"
        ));
    }
    let stored_crc = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let payload = &bytes[HEADER_BYTES..HEADER_BYTES + payload_len];
    let computed_crc = record_crc(&bytes[0..16], payload);
    if stored_crc != computed_crc {
        return Err(format!(
            "record crc32 mismatch: stored={stored_crc:#010x} computed={computed_crc:#010x} \
             payload_len={payload_len}; the record is torn or partially applied"
        ));
    }
    if payload_len == 0 {
        return Ok(ProjectionRecord::Vacant);
    }
    Ok(ProjectionRecord::Present(payload.to_vec()))
}

/// Owns the backing file for one projection and publishes records into it in
/// place.
///
/// The handle is held across publishes on purpose: an `open` per publish is
/// the cost this type exists to remove, and holding it makes a re-open visible
/// in [`PublishTimings::reopened`] rather than invisible in an average.
#[derive(Debug)]
pub(crate) struct ProjectionSlot {
    kind: ProjectionKind,
    path: PathBuf,
    file: Option<File>,
}

impl ProjectionSlot {
    pub(crate) fn new(kind: ProjectionKind, path: PathBuf) -> Self {
        Self {
            kind,
            path,
            file: None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Drops the cached handle so the next publish reopens.
    ///
    /// Called at the runtime Ledger-reconciliation boundary, where a free
    /// function may republish this file from recovered physical truth without
    /// going through this slot.
    pub(crate) fn reset(&mut self) {
        self.file = None;
    }

    /// Publishes one record in place, optionally forcing it durable.
    ///
    /// `durable` is false on the commit path — the payload is derived from
    /// Ledger rows the WAL has already fsync'd, so a barrier here can only
    /// make a regenerable projection slower (#1946). It is true on the
    /// recovery path, which runs once per open and leaves a known-good record
    /// on disk for the next crash to start from.
    pub(crate) fn publish(
        &mut self,
        payload: Option<&[u8]>,
        durable: bool,
    ) -> Result<PublishTimings> {
        let record = encode_record(self.kind, payload, &self.path)?;
        let mut timings = PublishTimings::default();

        let open_started = std::time::Instant::now();
        let created_now = if self.file.is_none() {
            timings.reopened = true;
            let created = self.open_backing_file()?;
            timings.open_us = elapsed_us(&open_started);
            created
        } else {
            false
        };

        let write_started = std::time::Instant::now();
        let write_result = self.write_record_at_zero(&record);
        timings.write_us = elapsed_us(&write_started);
        if let Err(error) = write_result {
            // A failed write leaves the handle's usability unknown. Drop it so
            // the next publish reopens rather than reusing a handle whose state
            // is only assumed to be good.
            self.file = None;
            return Err(error);
        }

        if durable {
            let sync_started = std::time::Instant::now();
            let sync_result = self.sync_backing_file();
            timings.sync_us = elapsed_us(&sync_started);
            if let Err(error) = sync_result {
                self.file = None;
                return Err(error);
            }
            // The record's *name* is only new on the publish that created the
            // file. That is the one case where the parent directory entry has
            // to reach disk for the record to be findable at all; every later
            // publish leaves the namespace untouched, which is the whole point
            // of this shape.
            if created_now {
                crate::fsync::sync_parent(&self.path, self.kind.label())?;
            }
        }
        Ok(timings)
    }

    /// Opens (creating and pre-allocating if needed) the backing file.
    ///
    /// Returns whether the file was created by this call.
    fn open_backing_file(&mut self) -> Result<bool> {
        let label = self.kind.label();
        if let Some(parent) = self.path.parent() {
            crate::fsync::create_dir_all(parent, label)?;
        }
        let existed = self.path.exists();
        let file = self.open_options().open(&self.path).map_err(|error| {
            projection_error(
                label,
                "open projection record",
                &self.path,
                io_detail(&error),
            )
        })?;
        let length = file.metadata().map(|meta| meta.len()).map_err(|error| {
            projection_error(
                label,
                "stat projection record",
                &self.path,
                io_detail(&error),
            )
        })?;
        // Pre-allocate exactly once. After this the length is invariant, so no
        // publish ever updates the file size — the metadata write this design
        // exists to avoid.
        if length != PROJECTION_RECORD_BYTES as u64 {
            file.set_len(PROJECTION_RECORD_BYTES as u64)
                .map_err(|error| {
                    projection_error(
                        label,
                        "pre-allocate projection record",
                        &self.path,
                        io_detail(&error),
                    )
                })?;
        }
        self.file = Some(file);
        Ok(!existed)
    }

    #[cfg(windows)]
    fn open_options(&self) -> OpenOptions {
        use std::os::windows::fs::OpenOptionsExt;

        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        // Readers and the backup copier need FILE_SHARE_READ. Delete is
        // deliberately NOT shared: nothing in this tree unlinks or renames a
        // projection any more, so an attempt to do so is an unaccounted path
        // and must fail loudly at the attempt rather than silently leave this
        // handle writing into an unlinked file that no reader can ever see.
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        options
    }

    #[cfg(not(windows))]
    fn open_options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        options
    }

    /// Writes the whole record at offset 0 without moving a file cursor.
    ///
    /// Positional writes keep this to one syscall and make the publish
    /// independent of any cursor state, so a partial write cannot leave the
    /// next publish writing at the wrong offset.
    fn write_record_at_zero(&self, record: &[u8; PROJECTION_RECORD_BYTES]) -> Result<()> {
        let label = self.kind.label();
        let path = self.path.as_path();
        let file = self.file.as_ref().ok_or_else(|| {
            projection_error(
                label,
                "write projection record",
                path,
                "backing file handle is absent".to_owned(),
            )
        })?;
        let mut written = 0_usize;
        while written < record.len() {
            let count =
                positional_write(file, &record[written..], written as u64).map_err(|error| {
                    projection_error(
                        label,
                        "write projection record",
                        path,
                        format!(
                            "{} after {written} of {} bytes",
                            io_detail(&error),
                            record.len()
                        ),
                    )
                })?;
            if count == 0 {
                return Err(projection_error(
                    label,
                    "write projection record",
                    path,
                    format!(
                        "positional write returned 0 after {written} of {} bytes",
                        record.len()
                    ),
                ));
            }
            written += count;
        }
        Ok(())
    }

    fn sync_backing_file(&self) -> Result<()> {
        let label = self.kind.label();
        let path = self.path.as_path();
        let file = self.file.as_ref().ok_or_else(|| {
            projection_error(
                label,
                "fsync projection record",
                path,
                "backing file handle is absent".to_owned(),
            )
        })?;
        // `sync_data` rather than `sync_all`: the length and the name are
        // already established and never change again, so only the record's
        // own bytes need a barrier.
        file.sync_data().map_err(|error| {
            projection_error(label, "fsync projection record", path, io_detail(&error))
        })
    }
}

#[cfg(windows)]
fn positional_write(file: &File, buffer: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;

    file.seek_write(buffer, offset)
}

#[cfg(unix)]
fn positional_write(file: &File, buffer: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;

    file.write_at(buffer, offset)
}

#[cfg(not(any(windows, unix)))]
fn positional_write(_file: &File, _buffer: &[u8], _offset: u64) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positional writes are unsupported on this platform",
    ))
}

fn elapsed_us(started: &std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
