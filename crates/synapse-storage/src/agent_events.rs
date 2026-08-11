//! `CF_AGENT_EVENTS` key codec (#897).
//!
//! Keys are `ts_ns (8 bytes BE) || seq (4 bytes BE)` — the same shape as
//! `CF_TIMELINE` — so rows iterate in chronological order, time-range scans
//! use the fixed 8-byte prefix extractor, and the GC engine's oldest-first
//! eviction works unchanged. `seq` is a process-wide monotonic counter that
//! breaks same-nanosecond ties; ordering authority within one tick is the
//! sequence, never the wall clock. Every producer and consumer must encode
//! and decode keys through this module so a malformed key is a structured
//! error, never a silent skip.
//!
//! Durability contract (#897 acceptance): journal rows use `Db::put_batch`,
//! which returns only after the row reaches the Calyx vault and its WAL.
//! Writers of terminal lifecycle events (exited/killed/spawn failure) also
//! call `Db::flush()` at the lifecycle boundary.

use crate::{StorageError, StorageResult, cf};

/// Encoded key length: 8-byte timestamp plus 4-byte sequence.
pub const AGENT_EVENT_KEY_LEN: usize = 12;

/// Maximum durable agent identifier length accepted by `AgentEventRecord`.
/// Kept here as the byte-level codec bound; identifiers are visible ASCII, so
/// the record's character and encoded-byte lengths are identical.
const AGENT_EVENT_INDEX_MAX_SPAWN_BYTES: usize = 512;

/// Encodes a `CF_AGENT_EVENTS` row key.
#[must_use]
pub fn agent_event_key(ts_ns: u64, seq: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(AGENT_EVENT_KEY_LEN);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

/// Encodes the inclusive scan start key for a timestamp.
#[must_use]
pub fn agent_event_scan_start(ts_ns: u64) -> Vec<u8> {
    agent_event_key(ts_ns, 0)
}

/// Decodes a `CF_AGENT_EVENTS` row key into `(ts_ns, seq)`.
///
/// # Errors
///
/// Returns [`StorageError::ReadFailed`] when the key is not exactly
/// [`AGENT_EVENT_KEY_LEN`] bytes.
pub fn decode_agent_event_key(key: &[u8]) -> StorageResult<(u64, u32)> {
    if key.len() != AGENT_EVENT_KEY_LEN {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENTS.to_owned(),
            detail: format!(
                "AGENT_EVENT_KEY_INVALID: expected {AGENT_EVENT_KEY_LEN} bytes, got {}",
                key.len()
            ),
        });
    }
    let (ts_bytes, seq_bytes) = key.split_at(8);
    let ts_ns = u64::from_be_bytes(ts_bytes.try_into().map_err(|_e| StorageError::ReadFailed {
        cf_name: cf::CF_AGENT_EVENTS.to_owned(),
        detail: "AGENT_EVENT_KEY_INVALID: timestamp bytes unreadable".to_owned(),
    })?);
    let seq = u32::from_be_bytes(
        seq_bytes
            .try_into()
            .map_err(|_e| StorageError::ReadFailed {
                cf_name: cf::CF_AGENT_EVENTS.to_owned(),
                detail: "AGENT_EVENT_KEY_INVALID: sequence bytes unreadable".to_owned(),
            })?,
    );
    Ok((ts_ns, seq))
}

/// Exact prefix for one spawn in `CF_AGENT_EVENT_SPAWN_INDEX`.
///
/// A length frame, rather than a delimiter, makes every visible-ASCII spawn id
/// prefix-safe even when it contains punctuation.
///
/// # Errors
///
/// Returns a structured read failure when the identifier is empty, over the
/// durable record bound, or contains bytes outside visible ASCII.
pub fn agent_event_spawn_index_prefix(spawn_id: &str) -> StorageResult<Vec<u8>> {
    let bytes = spawn_id.as_bytes();
    if bytes.is_empty()
        || bytes.len() > AGENT_EVENT_INDEX_MAX_SPAWN_BYTES
        || !bytes.iter().all(|byte| (b'!'..=b'~').contains(byte))
    {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
            detail: format!(
                "AGENT_EVENT_SPAWN_INDEX_ID_INVALID: spawn_id must be 1..={AGENT_EVENT_INDEX_MAX_SPAWN_BYTES} visible ASCII bytes, got len={}",
                bytes.len()
            ),
        });
    }
    let len = u16::try_from(bytes.len()).map_err(|_error| StorageError::ReadFailed {
        cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
        detail: format!(
            "AGENT_EVENT_SPAWN_INDEX_ID_INVALID: spawn_id length {} does not fit u16",
            bytes.len()
        ),
    })?;
    let mut prefix = Vec::with_capacity(2 + bytes.len());
    prefix.extend_from_slice(&len.to_be_bytes());
    prefix.extend_from_slice(bytes);
    Ok(prefix)
}

/// Encodes one spawn-index key, preserving the primary journal key order.
///
/// # Errors
///
/// Returns a structured read failure when the spawn id or source journal key
/// violates its canonical codec.
pub fn agent_event_spawn_index_key(spawn_id: &str, source_key: &[u8]) -> StorageResult<Vec<u8>> {
    let _identity = decode_agent_event_key(source_key)?;
    let mut key = agent_event_spawn_index_prefix(spawn_id)?;
    key.extend_from_slice(source_key);
    Ok(key)
}

/// Decodes and validates one complete spawn-index key.
///
/// # Errors
///
/// Returns a structured read failure for an invalid length frame, spawn id, or
/// source journal-key suffix.
pub fn decode_agent_event_spawn_index_key(key: &[u8]) -> StorageResult<(String, Vec<u8>)> {
    if key.len() < 2 + AGENT_EVENT_KEY_LEN {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
            detail: format!(
                "AGENT_EVENT_SPAWN_INDEX_KEY_INVALID: key is {} bytes, shorter than the framed minimum {}",
                key.len(),
                2 + AGENT_EVENT_KEY_LEN
            ),
        });
    }
    let spawn_len = usize::from(u16::from_be_bytes([key[0], key[1]]));
    let expected_len = 2_usize
        .checked_add(spawn_len)
        .and_then(|value| value.checked_add(AGENT_EVENT_KEY_LEN))
        .ok_or_else(|| StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
            detail: "AGENT_EVENT_SPAWN_INDEX_KEY_INVALID: framed key length overflow".to_owned(),
        })?;
    if spawn_len == 0 || spawn_len > AGENT_EVENT_INDEX_MAX_SPAWN_BYTES || key.len() != expected_len
    {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
            detail: format!(
                "AGENT_EVENT_SPAWN_INDEX_KEY_INVALID: framed spawn_len={spawn_len} total_len={} expected_total_len={expected_len}",
                key.len()
            ),
        });
    }
    let spawn_bytes = &key[2..2 + spawn_len];
    if !spawn_bytes.iter().all(|byte| (b'!'..=b'~').contains(byte)) {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
            detail:
                "AGENT_EVENT_SPAWN_INDEX_KEY_INVALID: spawn id contains non-visible-ASCII bytes"
                    .to_owned(),
        });
    }
    let spawn_id = String::from_utf8(spawn_bytes.to_vec()).map_err(|error| {
        StorageError::ReadFailed {
            cf_name: cf::CF_AGENT_EVENT_SPAWN_INDEX.to_owned(),
            detail: format!(
                "AGENT_EVENT_SPAWN_INDEX_KEY_INVALID: spawn id is not UTF-8 despite ASCII validation: {error}"
            ),
        }
    })?;
    let source_key = key[2 + spawn_len..].to_vec();
    let _identity = decode_agent_event_key(&source_key)?;
    Ok((spawn_id, source_key))
}
