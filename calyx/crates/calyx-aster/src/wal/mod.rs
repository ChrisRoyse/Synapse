//! Write-ahead log storage for Aster.

mod batch;
mod point_read;
mod record;
mod replay;
mod segment;
mod stream_replay;

use calyx_core::{CalyxError, CalyxErrorCode, Result};
use record::DecodeStatus;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use batch::GroupCommitBatcher;
pub(crate) use point_read::WalWriteRowPointReader;
pub use replay::replay_dir;
pub use replay::replay_dir_after;
pub(crate) use stream_replay::{
    for_each_record_payload_after, for_each_record_payload_reverse, stream_records_after,
};

pub(crate) const RECORD_HEADER_BYTES: u64 = record::HEADER_LEN as u64;

/// Default group-commit window for PH05.
pub const DEFAULT_GROUP_COMMIT_WINDOW: Duration = Duration::from_millis(2);
/// Maximum encoded payload accepted by one WAL record.
///
/// Public so embedding runtimes can preflight and chunk durable write batches
/// before they reach the fail-closed WAL encoder.
pub const MAX_RECORD_BYTES: usize = record::MAX_RECORD_BYTES as usize;

/// Opens WAL scan handles with the platform's sequential-read hint. Recovery,
/// streaming, and inventory all consume record framing monotonically.
fn sequential_read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN;

        options.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    options
}

/// WAL writer configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalOptions {
    /// Maximum bytes in one segment before the next append rotates.
    pub max_segment_bytes: u64,
    /// Upper bound for coalescing near-following requests into one fsync.
    pub group_commit_window: Duration,
}

impl Default for WalOptions {
    fn default() -> Self {
        Self {
            max_segment_bytes: 64 * 1024 * 1024,
            group_commit_window: DEFAULT_GROUP_COMMIT_WINDOW,
        }
    }
}

/// Fsync-backed append acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendAck {
    pub seq: u64,
    pub segment_path: PathBuf,
    pub start_offset: u64,
    pub end_offset: u64,
}

/// A replayed WAL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRecord {
    pub seq: u64,
    pub payload: Vec<u8>,
    pub segment_path: PathBuf,
    pub start_offset: u64,
    pub end_offset: u64,
}

/// Torn WAL tail discarded during replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TornTail {
    pub segment_path: PathBuf,
    pub offset: u64,
    pub code: &'static str,
    pub message: String,
}

impl TornTail {
    /// Converts the recovery observation to the catalogued Calyx error.
    pub fn error(&self) -> CalyxError {
        CalyxErrorCode::AsterTornWal.error(format!(
            "{} at byte {}: {}",
            self.segment_path.display(),
            self.offset,
            self.message
        ))
    }
}

/// WAL replay result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayOutcome {
    pub records: Vec<ReplayRecord>,
    pub torn_tail: Option<TornTail>,
}

/// Physical WAL segment readback used by recyclers and FSV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalSegmentStatus {
    pub index: u64,
    pub path: PathBuf,
    pub bytes: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub record_count: usize,
    pub active: bool,
}

/// Result of one bounded WAL recycle pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalRecycleReport {
    pub newest_durable_seq: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub segments_before: usize,
    pub recyclable_segments_before: usize,
    pub segments_recycled: usize,
    pub bytes_recycled: u64,
    pub recycled_paths: Vec<PathBuf>,
}

/// Durable WAL writer.
#[derive(Debug)]
pub struct Wal {
    dir: PathBuf,
    options: WalOptions,
    active_index: u64,
    file: File,
    active_len: u64,
    next_seq: u64,
}

impl Wal {
    /// Opens a WAL directory, replaying and truncating any torn tail first.
    pub fn open(dir: impl AsRef<Path>, options: WalOptions) -> Result<Self> {
        Self::open_after(dir, options, 0)
    }

    /// Opens a WAL directory while skipping payload decode for records already
    /// covered by a durable checkpoint sequence.
    pub(crate) fn open_after(
        dir: impl AsRef<Path>,
        options: WalOptions,
        replay_floor_seq: u64,
    ) -> Result<Self> {
        batch::validate_window(options.group_commit_window)?;
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|error| storage_error("create WAL directory", error))?;
        let _lock = crate::file_lock::FileLockGuard::acquire(&dir.join(".append.lock"))?;
        // Opening an append handle needs only the physical tip sequence, not a
        // second decoded copy of every post-checkpoint payload.
        let last_replayed_seq = recover_tip_seq_locked(&dir)?.max(replay_floor_seq);
        let next_seq = last_replayed_seq.saturating_add(1).max(1);
        let segments = segment::list_segments(&dir)?;
        let active_index = segments.last().map_or(0, |(index, _)| *index);
        let active_path = segment::segment_path(&dir, active_index);
        let mut file = open_append_file(&active_path)?;
        let active_len = file
            .seek(SeekFrom::End(0))
            .map_err(|error| storage_error("seek WAL segment", error))?;

        Ok(Self {
            dir,
            options,
            active_index,
            file,
            active_len,
            next_seq,
        })
    }

    /// Appends one record and fsyncs it.
    pub fn append(&mut self, payload: &[u8]) -> Result<AppendAck> {
        let mut acks = self.append_batch(&[payload])?;
        Ok(acks.remove(0))
    }

    /// Appends a batch and fsyncs once before acknowledging the records.
    pub fn append_batch(&mut self, payloads: &[&[u8]]) -> Result<Vec<AppendAck>> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }

        let _lock = crate::file_lock::FileLockGuard::acquire(&self.dir.join(".append.lock"))?;
        self.refresh_after_external_appends_locked()?;
        let mut acks = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let seq = self.next_seq;
            let bytes = record::encode(seq, payload)
                .map_err(|error| storage_error("encode WAL record", error))?;
            self.rotate_if_needed(bytes.len() as u64)?;
            let start_offset = self.seek_end()?;
            self.file
                .write_all(&bytes)
                .map_err(|error| storage_error("append WAL record", error))?;
            let end_offset = start_offset + bytes.len() as u64;
            acks.push(AppendAck {
                seq,
                segment_path: self.active_path(),
                start_offset,
                end_offset,
            });
            self.next_seq += 1;
            self.active_len = end_offset;
        }

        self.file
            .sync_data()
            .map_err(|error| storage_error("fsync WAL batch", error))?;
        Ok(acks)
    }

    pub fn durable_tip_seq(&mut self) -> Result<u64> {
        let _lock = crate::file_lock::FileLockGuard::acquire(&self.dir.join(".append.lock"))?;
        self.refresh_after_external_appends_locked()?;
        Ok(self.next_seq.saturating_sub(1))
    }

    /// Reads the physical WAL segment inventory behind the append lock.
    pub fn segment_inventory(&mut self) -> Result<Vec<WalSegmentStatus>> {
        let _lock = crate::file_lock::FileLockGuard::acquire(&self.dir.join(".append.lock"))?;
        self.refresh_after_external_appends_locked()?;
        segment_inventory_locked(&self.dir, self.active_index)
    }

    /// Returns total bytes in all canonical WAL segment files.
    pub fn total_segment_bytes(&mut self) -> Result<u64> {
        Ok(self
            .segment_inventory()?
            .iter()
            .map(|segment| segment.bytes)
            .sum())
    }

    /// Resets durable, non-active segments without deallocating segment files.
    pub fn recycle_durable_segments(
        &mut self,
        newest_durable_seq: u64,
        max_segments: usize,
        fsync_budget: usize,
    ) -> Result<WalRecycleReport> {
        let _lock = crate::file_lock::FileLockGuard::acquire(&self.dir.join(".append.lock"))?;
        self.refresh_after_external_appends_locked()?;
        let before = segment_inventory_locked(&self.dir, self.active_index)?;
        let bytes_before = total_bytes(&before);
        let budget = max_segments.min(fsync_budget);
        let candidates = recyclable_segments(&before, newest_durable_seq);
        let mut report = WalRecycleReport {
            newest_durable_seq,
            bytes_before,
            bytes_after: bytes_before,
            segments_before: before.len(),
            recyclable_segments_before: candidates.len(),
            ..WalRecycleReport::default()
        };
        if budget == 0 {
            return Ok(report);
        }

        for segment in candidates.into_iter().take(budget) {
            let file = OpenOptions::new()
                .write(true)
                .open(&segment.path)
                .map_err(|error| storage_error("open WAL segment for recycle", error))?;
            file.set_len(0)
                .map_err(|error| storage_error("truncate recycled WAL segment", error))?;
            file.sync_data()
                .map_err(|error| storage_error("fsync recycled WAL segment", error))?;
            report.segments_recycled += 1;
            report.bytes_recycled = report.bytes_recycled.saturating_add(segment.bytes);
            report.recycled_paths.push(segment.path.clone());
        }

        let after = segment_inventory_locked(&self.dir, self.active_index)?;
        report.bytes_after = total_bytes(&after);
        Ok(report)
    }

    fn refresh_after_external_appends_locked(&mut self) -> Result<()> {
        if !self.external_appends_present()? {
            return Ok(());
        }
        self.next_seq = recover_tip_seq_locked(&self.dir)?.saturating_add(1).max(1);
        let segments = segment::list_segments(&self.dir)?;
        let active_index = segments.last().map_or(0, |(index, _)| *index);
        if active_index != self.active_index {
            self.active_index = active_index;
            self.file = open_append_file(&self.active_path())?;
        }
        self.active_len = self.seek_end()?;
        Ok(())
    }

    fn external_appends_present(&self) -> Result<bool> {
        let segments = segment::list_segments(&self.dir)?;
        let Some((active_index, active_path)) = segments.last() else {
            return Ok(self.active_index != 0 || self.active_len != 0);
        };
        if *active_index != self.active_index {
            return Ok(true);
        }
        let len = fs::metadata(active_path)
            .map_err(|error| storage_error("stat WAL segment", error))?
            .len();
        Ok(len != self.active_len)
    }

    fn rotate_if_needed(&mut self, incoming_bytes: u64) -> Result<()> {
        let offset = self.seek_end()?;
        if offset == 0 || offset + incoming_bytes <= self.options.max_segment_bytes {
            return Ok(());
        }

        self.file
            .sync_all()
            .map_err(|error| storage_error("fsync WAL segment before rotation", error))?;
        self.active_index += 1;
        self.file = open_append_file(&self.active_path())?;
        self.active_len = 0;
        Ok(())
    }

    fn seek_end(&mut self) -> Result<u64> {
        self.file
            .seek(SeekFrom::End(0))
            .map_err(|error| storage_error("seek WAL segment", error))
    }

    fn active_path(&self) -> PathBuf {
        segment::segment_path(&self.dir, self.active_index)
    }
}

fn open_append_file(path: &Path) -> Result<File> {
    let existed = path.exists();
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|error| storage_error("open WAL segment", error))?;
    if !existed {
        sync_parent(path)?;
    }
    Ok(file)
}

fn sync_parent(path: &Path) -> Result<()> {
    crate::fsync::sync_parent(path, "WAL segment")
}

fn segment_inventory_locked(dir: &Path, active_index: u64) -> Result<Vec<WalSegmentStatus>> {
    let mut inventory = Vec::new();
    for (index, path) in segment::list_segments(dir)? {
        let bytes = fs::metadata(&path)
            .map_err(|error| storage_error("stat WAL segment", error))?
            .len();
        let (first_seq, last_seq, record_count) = read_segment_seq_bounds(&path)?;
        inventory.push(WalSegmentStatus {
            index,
            path,
            bytes,
            first_seq,
            last_seq,
            record_count,
            active: index == active_index,
        });
    }
    Ok(inventory)
}

fn read_segment_seq_bounds(path: &Path) -> Result<(Option<u64>, Option<u64>, usize)> {
    let file = sequential_read_options()
        .open(path)
        .map_err(|error| storage_error("open WAL segment for inventory", error))?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut offset = 0;
    let mut first = None;
    let mut last = None;
    let mut count = 0;
    loop {
        match record::decode_next(&mut reader, offset)
            .map_err(|error| storage_error("decode WAL inventory", error))?
        {
            DecodeStatus::Complete(decoded) => {
                first.get_or_insert(decoded.seq);
                last = Some(decoded.seq);
                count += 1;
                offset = decoded.end_offset;
            }
            DecodeStatus::Eof => return Ok((first, last, count)),
            DecodeStatus::Torn { offset, message } => {
                return Err(CalyxError::aster_torn_wal(format!(
                    "{} at byte {offset}: {message}",
                    path.display()
                )));
            }
        }
    }
}

/// Recovers the highest committed sequence using record headers only. The
/// newest physical torn tail is truncated exactly at its record boundary;
/// corruption in an older segment fails closed because later commits exist.
fn recover_tip_seq_locked(dir: &Path) -> Result<u64> {
    Ok(recover_tip_locked(dir)?.0)
}

fn recover_tip_locked(dir: &Path) -> Result<(u64, Option<TornTail>)> {
    let segments = segment::list_segments(dir)?;
    for (position, (_, path)) in segments.iter().enumerate().rev() {
        let has_later_segments = position + 1 < segments.len();
        let (last_seq, torn) = scan_segment_last_seq(path, has_later_segments)?;
        if let Some(seq) = last_seq {
            return Ok((seq, torn));
        }
        if torn.is_some() {
            continue;
        }
    }
    Ok((0, None))
}

/// Header-only WAL tip recovery for bounded vault-open planning. The later
/// streaming replay authenticates every covered and post-floor payload.
pub(crate) fn recover_tip_and_torn(dir: impl AsRef<Path>) -> Result<(u64, Option<TornTail>)> {
    let dir = dir.as_ref();
    let _lock = crate::file_lock::FileLockGuard::acquire(&dir.join(".append.lock"))?;
    recover_tip_locked(dir)
}

fn scan_segment_last_seq(
    path: &Path,
    has_later_segments: bool,
) -> Result<(Option<u64>, Option<TornTail>)> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| storage_error("open WAL segment for tip scan", error))?;
    let physical_len = file
        .metadata()
        .map_err(|error| storage_error("stat WAL segment for tip scan", error))?
        .len();
    let mut offset = 0;
    let mut last_seq = None;
    loop {
        match record::read_header_at(&mut file, offset)
            .map_err(|error| storage_error("scan WAL header", error))?
        {
            record::HeaderStatus::Complete(header) if header.end_offset <= physical_len => {
                last_seq = Some(header.seq);
                offset = header.end_offset;
            }
            record::HeaderStatus::Complete(header) => {
                return resolve_tip_torn(
                    &file,
                    path,
                    header.start_offset,
                    format!(
                        "partial WAL payload for seq {}: record ends at {}, file ends at {physical_len}",
                        header.seq, header.end_offset
                    ),
                    has_later_segments,
                    last_seq,
                );
            }
            record::HeaderStatus::Eof => return Ok((last_seq, None)),
            record::HeaderStatus::Torn { offset, message } => {
                return resolve_tip_torn(
                    &file,
                    path,
                    offset,
                    message,
                    has_later_segments,
                    last_seq,
                );
            }
        }
    }
}

fn resolve_tip_torn(
    file: &File,
    path: &Path,
    offset: u64,
    message: String,
    has_later_segments: bool,
    last_seq: Option<u64>,
) -> Result<(Option<u64>, Option<TornTail>)> {
    if has_later_segments {
        return Err(CalyxError::aster_torn_wal(format!(
            "{} at byte {offset}: {message}",
            path.display()
        )));
    }
    let torn = TornTail {
        segment_path: path.to_path_buf(),
        offset,
        code: CalyxErrorCode::AsterTornWal.code(),
        message,
    };
    file.set_len(offset)
        .map_err(|error| storage_error("truncate torn WAL tail", error))?;
    file.sync_data()
        .map_err(|error| storage_error("fsync truncated WAL tail", error))?;
    Ok((last_seq, Some(torn)))
}

fn recyclable_segments(
    inventory: &[WalSegmentStatus],
    newest_durable_seq: u64,
) -> Vec<&WalSegmentStatus> {
    inventory
        .iter()
        .filter(|segment| {
            !segment.active
                && segment.bytes > 0
                && segment
                    .last_seq
                    .is_some_and(|last_seq| last_seq <= newest_durable_seq)
        })
        .collect()
}

fn total_bytes(inventory: &[WalSegmentStatus]) -> u64 {
    inventory.iter().map(|segment| segment.bytes).sum()
}

fn storage_error(context: &str, error: io::Error) -> CalyxError {
    CalyxError::disk_pressure(format!("{context}: {error}"))
}
