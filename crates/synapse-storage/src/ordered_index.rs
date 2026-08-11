//! Exact descending-order key codecs for durable secondary indexes (#2189,
//! #2190).
//!
//! Calyx range scans are ascending. Numeric fields are bitwise inverted and
//! strings use a prefix-safe, order-reversing u16 alphabet so ascending bytes
//! reproduce Rust's exact `(timestamp, string, ...)` descending comparator,
//! including variable-length prefix cases.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{StorageError, StorageResult, cf};

pub const ORDER_POINTER_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedSourcePointer {
    pub schema_version: u32,
    pub source_key_hex: String,
    pub source_value_sha256: String,
}

impl OrderedSourcePointer {
    #[must_use]
    pub fn new(source_key: &[u8], source_value: &[u8]) -> Self {
        Self {
            schema_version: ORDER_POINTER_VERSION,
            source_key_hex: hex_encode(source_key),
            source_value_sha256: sha256_hex(source_value),
        }
    }

    /// Decodes the exact source key named by this pointer.
    ///
    /// # Errors
    ///
    /// Returns a structured read failure when the pointer schema or source-key
    /// hex is invalid.
    pub fn source_key(&self, index_cf: &str) -> StorageResult<Vec<u8>> {
        if self.schema_version != ORDER_POINTER_VERSION {
            return Err(index_read_failed(
                index_cf,
                format!(
                    "ORDER_POINTER_SCHEMA_UNSUPPORTED: actual={} expected={ORDER_POINTER_VERSION}; remediation=inspect the index row and rebuild the unpublished projection",
                    self.schema_version
                ),
            ));
        }
        hex_decode(&self.source_key_hex).ok_or_else(|| {
            index_read_failed(
                index_cf,
                format!(
                    "ORDER_POINTER_SOURCE_KEY_INVALID: value={:?}; remediation=restore the exact source pointer or rebuild the unpublished projection",
                    self.source_key_hex
                ),
            )
        })
    }

    /// Verifies the independently read source bytes against this pointer.
    ///
    /// # Errors
    ///
    /// Returns a structured read failure when the source digest differs.
    pub fn verify_source_value(&self, index_cf: &str, value: &[u8]) -> StorageResult<()> {
        let actual = sha256_hex(value);
        if self.source_value_sha256 != actual {
            return Err(index_read_failed(
                index_cf,
                format!(
                    "ORDER_POINTER_SOURCE_DIGEST_MISMATCH: expected={} actual={actual}; remediation=preserve the source and index rows, inspect the divergent commit, and rebuild only after the cause is known",
                    self.source_value_sha256
                ),
            ));
        }
        Ok(())
    }
}

#[must_use]
pub fn transcript_order_key(ts_ns: u64, spawn_id: &str, line_no: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + (spawn_id.len() + 1) * 2 + 8);
    key.extend_from_slice(&(!ts_ns).to_be_bytes());
    encode_descending_bytes(spawn_id.as_bytes(), &mut key);
    key.extend_from_slice(&(!line_no).to_be_bytes());
    key
}

#[must_use]
pub fn reflex_audit_order_key(ts_ns: u64, audit_id: &str, reflex_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + (audit_id.len() + reflex_id.len() + 2) * 2);
    key.extend_from_slice(&(!ts_ns).to_be_bytes());
    encode_descending_bytes(audit_id.as_bytes(), &mut key);
    encode_descending_bytes(reflex_id.as_bytes(), &mut key);
    key
}

/// Decodes one strict ordered source pointer.
///
/// # Errors
///
/// Returns a structured read failure when JSON or schema fields are invalid.
pub fn decode_pointer(index_cf: &str, bytes: &[u8]) -> StorageResult<OrderedSourcePointer> {
    serde_json::from_slice(bytes).map_err(|error| {
        index_read_failed(
            index_cf,
            format!(
                "ORDER_POINTER_DECODE_FAILED: {error}; remediation=preserve the corrupt index row and rebuild only after identifying the writer or physical fault"
            ),
        )
    })
}

/// Encodes one strict ordered source pointer.
///
/// # Errors
///
/// Returns a storage encoding failure when JSON serialization fails.
pub fn encode_pointer(pointer: &OrderedSourcePointer) -> StorageResult<Vec<u8>> {
    serde_json::to_vec(pointer).map_err(|source| StorageError::EncodeJson {
        type_name: std::any::type_name::<OrderedSourcePointer>(),
        source,
    })
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[must_use]
pub fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

fn encode_descending_bytes(bytes: &[u8], output: &mut Vec<u8>) {
    for byte in bytes {
        let ascending_symbol = u16::from(*byte) + 1;
        output.extend_from_slice(&(u16::MAX - ascending_symbol).to_be_bytes());
    }
    // Ascending lexical encoding uses zero as a terminator. Complementing it
    // makes a shorter prefix sort after its extension, exactly reversing Ord.
    output.extend_from_slice(&u16::MAX.to_be_bytes());
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn index_read_failed(index_cf: &str, detail: String) -> StorageError {
    StorageError::ReadFailed {
        cf_name: if matches!(
            index_cf,
            cf::CF_AGENT_TRANSCRIPT_ORDER
                | cf::CF_REFLEX_AUDIT_ORDER
                | cf::CF_AGENT_EVENT_SPAWN_INDEX
        ) {
            index_cf.to_owned()
        } else {
            format!("INVALID_INDEX_CF:{index_cf}")
        },
        detail,
    }
}
