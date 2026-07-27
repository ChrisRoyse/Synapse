use std::io::BufReader;
use std::path::Path;

use calyx_core::{CalyxErrorCode, Result};

use super::record::{DecodeStatus, PayloadStatus};
use super::{ReplayRecord, TornTail, record, segment, storage_error};

const REPLAY_BUFFER_BYTES: usize = 1024 * 1024;

/// Visits WAL payloads above `replay_floor_seq` with one reused payload buffer.
/// Returning `false` stops the scan. Records at or below the floor are still
/// authenticated without retaining their payloads.
pub(crate) fn for_each_record_payload_after(
    dir: impl AsRef<Path>,
    replay_floor_seq: u64,
    mut visit: impl FnMut(u64, &[u8]) -> Result<bool>,
) -> Result<()> {
    let dir = dir.as_ref();
    let _lock = crate::file_lock::FileLockGuard::acquire(&dir.join(".append.lock"))?;
    let segments = segment::list_segments(dir)?;
    let mut payload = Vec::new();
    for (_, path) in segments {
        let file = super::sequential_read_options()
            .open(&path)
            .map_err(|error| storage_error("open WAL segment for bounded stream", error))?;
        let mut reader = BufReader::with_capacity(REPLAY_BUFFER_BYTES, file);
        let mut offset = 0;
        loop {
            let header = match record::read_header(&mut reader, offset)
                .map_err(|error| storage_error("decode WAL header", error))?
            {
                record::HeaderStatus::Complete(header) => header,
                record::HeaderStatus::Eof => break,
                record::HeaderStatus::Torn { offset, message } => {
                    return Err(torn_error(&path, offset, message));
                }
            };
            if header.seq <= replay_floor_seq {
                match record::validate_payload(&mut reader, &header)
                    .map_err(|error| storage_error("validate durable WAL payload", error))?
                {
                    PayloadStatus::Complete => offset = header.end_offset,
                    PayloadStatus::Torn { offset, message } => {
                        return Err(torn_error(&path, offset, message));
                    }
                }
                continue;
            }
            match record::decode_payload_into(&mut reader, header, &mut payload)
                .map_err(|error| storage_error("decode WAL payload", error))?
            {
                record::DecodeIntoStatus::Complete {
                    seq, end_offset, ..
                } => {
                    offset = end_offset;
                    if !visit(seq, &payload)? {
                        return Ok(());
                    }
                }
                record::DecodeIntoStatus::Torn { offset, message } => {
                    return Err(torn_error(&path, offset, message));
                }
            }
        }
    }
    Ok(())
}

/// Visits newest segments first and stops as soon as `visit` returns false.
/// Records within a segment remain in ascending physical order.
pub(crate) fn for_each_record_payload_reverse(
    dir: impl AsRef<Path>,
    mut visit: impl FnMut(u64, &[u8]) -> Result<bool>,
) -> Result<()> {
    let dir = dir.as_ref();
    let _lock = crate::file_lock::FileLockGuard::acquire(&dir.join(".append.lock"))?;
    let segments = segment::list_segments(dir)?;
    let mut payload = Vec::new();
    for (_, path) in segments.iter().rev() {
        let file = super::sequential_read_options()
            .open(path)
            .map_err(|error| storage_error("open WAL segment for reverse stream", error))?;
        let mut reader = BufReader::with_capacity(REPLAY_BUFFER_BYTES, file);
        let mut offset = 0;
        loop {
            let header = match record::read_header(&mut reader, offset)
                .map_err(|error| storage_error("decode WAL header", error))?
            {
                record::HeaderStatus::Complete(header) => header,
                record::HeaderStatus::Eof => break,
                record::HeaderStatus::Torn { offset, message } => {
                    return Err(torn_error(path, offset, message));
                }
            };
            match record::decode_payload_into(&mut reader, header, &mut payload)
                .map_err(|error| storage_error("decode WAL payload", error))?
            {
                record::DecodeIntoStatus::Complete {
                    seq, end_offset, ..
                } => {
                    offset = end_offset;
                    if !visit(seq, &payload)? {
                        return Ok(());
                    }
                }
                record::DecodeIntoStatus::Torn { offset, message } => {
                    return Err(torn_error(path, offset, message));
                }
            }
        }
    }
    Ok(())
}

fn torn_error(path: &Path, offset: u64, message: String) -> calyx_core::CalyxError {
    TornTail {
        segment_path: path.to_path_buf(),
        offset,
        code: CalyxErrorCode::AsterTornWal.code(),
        message,
    }
    .error()
}

/// Streams only records newer than durable SST coverage. Records at or below
/// the floor have their framing and checksums validated through a bounded,
/// sequential scan without retaining vector-heavy payloads.
pub(crate) fn stream_records_after(
    dir: impl AsRef<std::path::Path>,
    replay_floor_seq: u64,
    mut visit: impl FnMut(ReplayRecord) -> Result<()>,
) -> Result<usize> {
    let dir = dir.as_ref();
    let _lock = crate::file_lock::FileLockGuard::acquire(&dir.join(".append.lock"))?;
    let segments = segment::list_segments(dir)?;
    let mut count = 0;
    for (_, path) in segments {
        let file = super::sequential_read_options()
            .open(&path)
            .map_err(|error| storage_error("open WAL segment for stream replay", error))?;
        let mut reader = BufReader::with_capacity(REPLAY_BUFFER_BYTES, file);
        let mut offset = 0;
        loop {
            let header = match record::read_header(&mut reader, offset)
                .map_err(|error| storage_error("decode WAL header", error))?
            {
                record::HeaderStatus::Complete(header) => header,
                record::HeaderStatus::Eof => break,
                record::HeaderStatus::Torn { offset, message } => {
                    return Err(TornTail {
                        segment_path: path.clone(),
                        offset,
                        code: CalyxErrorCode::AsterTornWal.code(),
                        message,
                    }
                    .error());
                }
            };
            if header.seq <= replay_floor_seq {
                match record::validate_payload(&mut reader, &header)
                    .map_err(|error| storage_error("validate durable WAL payload", error))?
                {
                    PayloadStatus::Complete => offset = header.end_offset,
                    PayloadStatus::Torn { offset, message } => {
                        return Err(TornTail {
                            segment_path: path.clone(),
                            offset,
                            code: CalyxErrorCode::AsterTornWal.code(),
                            message,
                        }
                        .error());
                    }
                }
                continue;
            }
            match record::decode_payload(&mut reader, header)
                .map_err(|error| storage_error("decode WAL record", error))?
            {
                DecodeStatus::Complete(decoded) => {
                    offset = decoded.end_offset;
                    count += 1;
                    visit(ReplayRecord {
                        seq: decoded.seq,
                        payload: decoded.payload,
                        segment_path: path.clone(),
                        start_offset: decoded.start_offset,
                        end_offset: decoded.end_offset,
                    })?;
                }
                DecodeStatus::Eof => break,
                DecodeStatus::Torn { offset, message } => {
                    return Err(TornTail {
                        segment_path: path.clone(),
                        offset,
                        code: CalyxErrorCode::AsterTornWal.code(),
                        message,
                    }
                    .error());
                }
            }
        }
    }
    Ok(count)
}
