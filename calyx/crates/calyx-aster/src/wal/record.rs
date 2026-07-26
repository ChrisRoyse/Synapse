//! WAL record framing.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub(super) const MAGIC: u32 = u32::from_le_bytes(*b"CXW1");
pub(super) const HEADER_LEN: usize = 20;
pub(super) const MAX_RECORD_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DecodeStatus {
    Complete(DecodedRecord),
    Eof,
    Torn { offset: u64, message: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum HeaderStatus {
    Complete(RecordHeader),
    Eof,
    Torn { offset: u64, message: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PayloadStatus {
    Complete,
    Torn { offset: u64, message: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RecordHeader {
    pub seq: u64,
    pub len: u32,
    pub expected_crc: u32,
    pub start_offset: u64,
    pub end_offset: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DecodedRecord {
    pub seq: u64,
    pub payload: Vec<u8>,
    pub start_offset: u64,
    pub end_offset: u64,
}

pub(super) fn encode(seq: u64, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > MAX_RECORD_BYTES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("WAL payload exceeds max record size {MAX_RECORD_BYTES}"),
        ));
    }

    let len = payload.len() as u32;
    let crc = payload_crc(seq, len, payload);
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
    bytes.extend_from_slice(&MAGIC.to_le_bytes());
    bytes.extend_from_slice(&seq.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

pub(super) fn decode_next(reader: &mut impl Read, offset: u64) -> io::Result<DecodeStatus> {
    let header = match read_header(reader, offset)? {
        HeaderStatus::Complete(header) => header,
        HeaderStatus::Eof => return Ok(DecodeStatus::Eof),
        HeaderStatus::Torn { offset, message } => {
            return Ok(DecodeStatus::Torn { offset, message });
        }
    };
    decode_payload(reader, header)
}

pub(super) fn decode_payload(
    reader: &mut impl Read,
    header: RecordHeader,
) -> io::Result<DecodeStatus> {
    let mut payload = vec![0u8; header.len as usize];
    if let Err(error) = reader.read_exact(&mut payload) {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            return Ok(DecodeStatus::Torn {
                offset: header.start_offset,
                message: format!(
                    "partial WAL payload for seq {}: wanted {} bytes",
                    header.seq, header.len
                ),
            });
        }
        return Err(error);
    }

    let actual_crc = payload_crc(header.seq, header.len, &payload);
    if actual_crc != header.expected_crc {
        return Ok(DecodeStatus::Torn {
            offset: header.start_offset,
            message: format!(
                "crc mismatch for seq {}: expected {:08x}, got {actual_crc:08x}",
                header.seq, header.expected_crc
            ),
        });
    }

    Ok(DecodeStatus::Complete(DecodedRecord {
        seq: header.seq,
        payload,
        start_offset: header.start_offset,
        end_offset: header.end_offset,
    }))
}

/// Reads and authenticates a payload without retaining it. Recovery uses this
/// for records already represented by durable SST state so the WAL is scanned
/// sequentially with bounded memory while corruption still fails closed.
pub(super) fn validate_payload(
    reader: &mut impl Read,
    header: &RecordHeader,
) -> io::Result<PayloadStatus> {
    const SCRATCH_BYTES: usize = 64 * 1024;

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&header.seq.to_le_bytes());
    hasher.update(&header.len.to_le_bytes());
    let mut remaining = header.len as usize;
    let mut scratch = [0_u8; SCRATCH_BYTES];
    while remaining > 0 {
        let wanted = remaining.min(scratch.len());
        let read = match reader.read(&mut scratch[..wanted]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(PayloadStatus::Torn {
                offset: header.start_offset,
                message: format!(
                    "partial WAL payload for seq {}: wanted {} bytes",
                    header.seq, header.len
                ),
            });
        }
        hasher.update(&scratch[..read]);
        remaining -= read;
    }

    let actual_crc = hasher.finalize();
    if actual_crc != header.expected_crc {
        return Ok(PayloadStatus::Torn {
            offset: header.start_offset,
            message: format!(
                "crc mismatch for seq {}: expected {:08x}, got {actual_crc:08x}",
                header.seq, header.expected_crc
            ),
        });
    }
    Ok(PayloadStatus::Complete)
}

pub(super) fn read_header_at(file: &mut File, offset: u64) -> io::Result<HeaderStatus> {
    file.seek(SeekFrom::Start(offset))?;
    read_header(file, offset)
}

pub(super) fn read_header(reader: &mut impl Read, offset: u64) -> io::Result<HeaderStatus> {
    let mut header = [0u8; HEADER_LEN];
    let mut read = 0;
    while read < HEADER_LEN {
        match reader.read(&mut header[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    if read == 0 {
        return Ok(HeaderStatus::Eof);
    }
    if read < HEADER_LEN {
        return Ok(HeaderStatus::Torn {
            offset,
            message: format!("partial WAL header: {read}/{HEADER_LEN} bytes"),
        });
    }

    let magic = u32::from_le_bytes(header[0..4].try_into().expect("magic width"));
    if magic != MAGIC {
        return Ok(HeaderStatus::Torn {
            offset,
            message: format!("bad WAL magic 0x{magic:08x}"),
        });
    }

    let seq = u64::from_le_bytes(header[4..12].try_into().expect("seq width"));
    let len = u32::from_le_bytes(header[12..16].try_into().expect("len width"));
    let expected_crc = u32::from_le_bytes(header[16..20].try_into().expect("crc width"));
    if len > MAX_RECORD_BYTES {
        return Ok(HeaderStatus::Torn {
            offset,
            message: format!("record length {len} exceeds max {MAX_RECORD_BYTES}"),
        });
    }
    Ok(HeaderStatus::Complete(RecordHeader {
        seq,
        len,
        expected_crc,
        start_offset: offset,
        end_offset: offset + HEADER_LEN as u64 + len as u64,
    }))
}

fn payload_crc(seq: u64, len: u32, payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&seq.to_le_bytes());
    hasher.update(&len.to_le_bytes());
    hasher.update(payload);
    hasher.finalize()
}
