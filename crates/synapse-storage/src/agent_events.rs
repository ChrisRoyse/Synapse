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
//! which returns only after the row reaches `RocksDB` with a synced WAL.
//! Writers of terminal lifecycle events (exited/killed/spawn failure) also
//! call `Db::flush()` at the lifecycle boundary.

use crate::{StorageError, StorageResult, cf};

/// Encoded key length: 8-byte timestamp plus 4-byte sequence.
pub const AGENT_EVENT_KEY_LEN: usize = 12;

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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn key_orders_chronologically_then_by_seq() {
        let earlier = agent_event_key(1_000, u32::MAX);
        let later = agent_event_key(1_001, 0);
        assert!(earlier < later, "timestamp must dominate ordering");
        let first = agent_event_key(1_000, 1);
        let second = agent_event_key(1_000, 2);
        assert!(first < second, "sequence must break same-tick ties");
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let error = decode_agent_event_key(&[0_u8; 11]).expect_err("11 bytes must be rejected");
        assert!(
            error.to_string().contains("AGENT_EVENT_KEY_INVALID"),
            "error must carry the structured detail: {error}"
        );
    }

    proptest! {
        #[test]
        fn key_roundtrip(ts_ns in any::<u64>(), seq in any::<u32>()) {
            let key = agent_event_key(ts_ns, seq);
            prop_assert_eq!(key.len(), AGENT_EVENT_KEY_LEN);
            let (decoded_ts, decoded_seq) = decode_agent_event_key(&key).expect("roundtrip");
            prop_assert_eq!(decoded_ts, ts_ns);
            prop_assert_eq!(decoded_seq, seq);
        }
    }
}
