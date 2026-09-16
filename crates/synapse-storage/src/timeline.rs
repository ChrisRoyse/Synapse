//! `CF_TIMELINE` key codec (ADR 2026-06-11-timeline-data-model).
//!
//! Keys are `ts_ns (8 bytes BE) || seq (4 bytes BE)` so rows iterate in
//! chronological order, time-range scans use the fixed 8-byte prefix
//! extractor, and the GC engine's oldest-first eviction works unchanged.
//! Every timeline producer and consumer must encode and decode keys through
//! this module so a malformed key is a structured error, never a silent skip.

use crate::{StorageError, StorageResult, cf};

/// Encoded key length: 8-byte timestamp plus 4-byte sequence.
pub const TIMELINE_KEY_LEN: usize = 12;

/// Encodes a `CF_TIMELINE` row key.
#[must_use]
pub fn timeline_key(ts_ns: u64, seq: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(TIMELINE_KEY_LEN);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

/// Encodes the inclusive scan start key for a timestamp.
#[must_use]
pub fn timeline_scan_start(ts_ns: u64) -> Vec<u8> {
    timeline_key(ts_ns, 0)
}

/// Decodes a `CF_TIMELINE` row key into `(ts_ns, seq)`.
///
/// # Errors
///
/// Returns [`StorageError::ReadFailed`] when the key is not exactly
/// [`TIMELINE_KEY_LEN`] bytes.
pub fn decode_timeline_key(key: &[u8]) -> StorageResult<(u64, u32)> {
    if key.len() != TIMELINE_KEY_LEN {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_TIMELINE.to_owned(),
            detail: format!(
                "TIMELINE_KEY_INVALID: expected {TIMELINE_KEY_LEN} bytes, got {}",
                key.len()
            ),
        });
    }
    let (ts_bytes, seq_bytes) = key.split_at(8);
    let ts_ns = u64::from_be_bytes(ts_bytes.try_into().map_err(|_e| StorageError::ReadFailed {
        cf_name: cf::CF_TIMELINE.to_owned(),
        detail: "TIMELINE_KEY_INVALID: timestamp bytes unreadable".to_owned(),
    })?);
    let seq = u32::from_be_bytes(
        seq_bytes
            .try_into()
            .map_err(|_e| StorageError::ReadFailed {
                cf_name: cf::CF_TIMELINE.to_owned(),
                detail: "TIMELINE_KEY_INVALID: sequence bytes unreadable".to_owned(),
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
        let earlier = timeline_key(1_000, u32::MAX);
        let later = timeline_key(1_001, 0);
        assert!(earlier < later, "timestamp must dominate ordering");
        let first = timeline_key(1_000, 1);
        let second = timeline_key(1_000, 2);
        assert!(first < second, "sequence must break same-tick ties");
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let error = decode_timeline_key(&[0_u8; 11]).expect_err("11 bytes must be rejected");
        assert!(
            error.to_string().contains("TIMELINE_KEY_INVALID"),
            "error must carry the structured detail: {error}"
        );
    }

    proptest! {
        #[test]
        fn key_roundtrip(ts_ns in any::<u64>(), seq in any::<u32>()) {
            let key = timeline_key(ts_ns, seq);
            prop_assert_eq!(key.len(), TIMELINE_KEY_LEN);
            let (decoded_ts, decoded_seq) = decode_timeline_key(&key).expect("roundtrip");
            prop_assert_eq!(decoded_ts, ts_ns);
            prop_assert_eq!(decoded_seq, seq);
        }
    }
}
