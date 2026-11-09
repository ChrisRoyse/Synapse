//! `CF_EPISODES` key codec (#846).
//!
//! Keys are `start_ts_ns (8 bytes BE) || ordinal (4 bytes BE)` — the same
//! shape as `CF_TIMELINE` keys, so chronological iteration, the 8-byte
//! fixed-prefix extractor, and the GC engine's oldest-first eviction all work
//! unchanged. The ordinal is the episode's index within its segmentation
//! day, which keeps keys unique when two episodes open on the same
//! nanosecond (zero-duration focus flickers).
//!
//! Every producer and consumer must go through this module so a malformed
//! key is a structured error, never a silent skip.

use crate::{StorageError, StorageResult, cf};

/// Encoded key length: 8-byte start timestamp plus 4-byte ordinal.
pub const EPISODE_KEY_LEN: usize = 12;

/// Encodes a `CF_EPISODES` row key.
#[must_use]
pub fn episode_key(start_ts_ns: u64, ordinal: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(EPISODE_KEY_LEN);
    key.extend_from_slice(&start_ts_ns.to_be_bytes());
    key.extend_from_slice(&ordinal.to_be_bytes());
    key
}

/// Encodes the inclusive scan start key for a timestamp.
#[must_use]
pub fn episode_scan_start(start_ts_ns: u64) -> Vec<u8> {
    episode_key(start_ts_ns, 0)
}

/// Decodes a `CF_EPISODES` row key into `(start_ts_ns, ordinal)`.
///
/// # Errors
///
/// Returns [`StorageError::ReadFailed`] when the key is not exactly
/// [`EPISODE_KEY_LEN`] bytes.
pub fn decode_episode_key(key: &[u8]) -> StorageResult<(u64, u32)> {
    if key.len() != EPISODE_KEY_LEN {
        return Err(StorageError::ReadFailed {
            cf_name: cf::CF_EPISODES.to_owned(),
            detail: format!(
                "EPISODE_KEY_INVALID: expected {EPISODE_KEY_LEN} bytes, got {}",
                key.len()
            ),
        });
    }
    let (ts_bytes, ordinal_bytes) = key.split_at(8);
    let start_ts_ns =
        u64::from_be_bytes(ts_bytes.try_into().map_err(|_e| StorageError::ReadFailed {
            cf_name: cf::CF_EPISODES.to_owned(),
            detail: "EPISODE_KEY_INVALID: timestamp bytes unreadable".to_owned(),
        })?);
    let ordinal =
        u32::from_be_bytes(
            ordinal_bytes
                .try_into()
                .map_err(|_e| StorageError::ReadFailed {
                    cf_name: cf::CF_EPISODES.to_owned(),
                    detail: "EPISODE_KEY_INVALID: ordinal bytes unreadable".to_owned(),
                })?,
        );
    Ok((start_ts_ns, ordinal))
}
