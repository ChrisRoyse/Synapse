use std::io::BufReader;

use calyx_core::{CalyxErrorCode, Result};

use super::record::{DecodeStatus, PayloadStatus};
use super::{ReplayRecord, TornTail, record, segment, storage_error};

const REPLAY_BUFFER_BYTES: usize = 1024 * 1024;

pub(crate) fn stream_records(
    dir: impl AsRef<std::path::Path>,
    visit: impl FnMut(ReplayRecord) -> Result<()>,
) -> Result<usize> {
    stream_records_after(dir, 0, visit)
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
