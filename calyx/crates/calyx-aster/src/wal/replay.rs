use super::record::{DecodeStatus, PayloadStatus};
use super::{ReplayOutcome, ReplayRecord, TornTail, record, segment, storage_error};
use calyx_core::{CalyxErrorCode, Result};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

const REPLAY_BUFFER_BYTES: usize = 1024 * 1024;

/// Replays a WAL directory, truncating a torn physical tail if present.
pub fn replay_dir(dir: impl AsRef<Path>) -> Result<ReplayOutcome> {
    replay_dir_after(dir, 0)
}

/// Replays WAL records after a durable checkpoint sequence.
///
/// Records at or below `replay_floor_seq` are already represented by durable
/// SST/manifest state. Recovery scans and authenticates their framing and
/// payloads sequentially without retaining old vector-heavy batches.
pub fn replay_dir_after(dir: impl AsRef<Path>, replay_floor_seq: u64) -> Result<ReplayOutcome> {
    let dir = dir.as_ref();
    let _lock = crate::file_lock::FileLockGuard::acquire(&dir.join(".append.lock"))?;
    replay_dir_inner(dir, replay_floor_seq, true)
}

/// Scans an immutable WAL snapshot without creating a lock file or repairing bytes.
pub fn replay_dir_read_only(dir: impl AsRef<Path>) -> Result<ReplayOutcome> {
    replay_dir_inner(dir.as_ref(), 0, false)
}

fn replay_dir_inner(
    dir: &Path,
    replay_floor_seq: u64,
    repair_torn_tail: bool,
) -> Result<ReplayOutcome> {
    let segments = segment::list_segments(dir)?;
    let mut records = Vec::new();

    for (position, (_, path)) in segments.iter().enumerate() {
        let has_later_segments = position + 1 < segments.len();
        let file = super::sequential_read_options()
            .write(repair_torn_tail)
            .open(path)
            .map_err(|error| storage_error("open WAL segment for replay", error))?;
        let mut reader = BufReader::with_capacity(REPLAY_BUFFER_BYTES, file);
        let mut offset = 0;

        loop {
            let header = match record::read_header(&mut reader, offset)
                .map_err(|error| storage_error("decode WAL header", error))?
            {
                record::HeaderStatus::Complete(header) => header,
                record::HeaderStatus::Eof => break,
                record::HeaderStatus::Torn { offset, message } => {
                    return resolve_torn_tail(
                        reader.get_ref(),
                        path,
                        offset,
                        message,
                        records,
                        has_later_segments,
                        repair_torn_tail,
                    );
                }
            };
            if header.seq <= replay_floor_seq {
                match record::validate_payload(&mut reader, &header)
                    .map_err(|error| storage_error("validate durable WAL payload", error))?
                {
                    PayloadStatus::Complete => offset = header.end_offset,
                    PayloadStatus::Torn { offset, message } => {
                        return resolve_torn_tail(
                            reader.get_ref(),
                            path,
                            offset,
                            message,
                            records,
                            has_later_segments,
                            repair_torn_tail,
                        );
                    }
                }
                continue;
            }
            match record::decode_payload(&mut reader, header)
                .map_err(|error| storage_error("decode WAL record", error))?
            {
                DecodeStatus::Complete(decoded) => {
                    offset = decoded.end_offset;
                    records.push(ReplayRecord {
                        seq: decoded.seq,
                        payload: decoded.payload,
                        segment_path: path.clone(),
                        start_offset: decoded.start_offset,
                        end_offset: decoded.end_offset,
                    });
                }
                DecodeStatus::Eof => break,
                DecodeStatus::Torn { offset, message } => {
                    return resolve_torn_tail(
                        reader.get_ref(),
                        path,
                        offset,
                        message,
                        records,
                        has_later_segments,
                        repair_torn_tail,
                    );
                }
            }
        }
    }

    Ok(ReplayOutcome {
        records,
        torn_tail: None,
    })
}

fn resolve_torn_tail(
    file: &File,
    segment_path: &Path,
    offset: u64,
    message: String,
    records: Vec<ReplayRecord>,
    has_later_segments: bool,
    repair_torn_tail: bool,
) -> Result<ReplayOutcome> {
    let torn_tail = TornTail {
        segment_path: segment_path.to_path_buf(),
        offset,
        code: CalyxErrorCode::AsterTornWal.code(),
        message,
    };
    if has_later_segments || !repair_torn_tail {
        return Err(torn_tail.error());
    }

    file.set_len(offset)
        .map_err(|error| storage_error("truncate torn WAL tail", error))?;
    file.sync_data()
        .map_err(|error| storage_error("fsync truncated WAL tail", error))?;
    Ok(ReplayOutcome {
        records,
        torn_tail: Some(torn_tail),
    })
}
