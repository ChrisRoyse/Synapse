use calyx_core::{CalyxError, CxId, Result};

use crate::cf::{KeyRange, prefix_range};

use super::types::DecodedEdge;

pub(super) const ID_BYTES: usize = 16;
pub(super) const MAX_TRAVERSE_HOPS: usize = 32;
pub(super) const MAX_TRAVERSE_COST: usize = 100_000;

const DISC: u8 = b'g';
const KIND_NODE: u8 = 0;
const KIND_EDGE_OUT: u8 = 1;
const KIND_EDGE_IN: u8 = 2;
const KIND_CSR: u8 = 3;
const KIND_METADATA: u8 = 4;
const KIND_CSR_SEGMENT: u8 = 5;
const MAX_COLLECTION_BYTES: usize = 256;
const MAX_EDGE_TYPE_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GraphKeyspace {
    collection: Vec<u8>,
}

impl GraphKeyspace {
    pub(super) fn new(collection: &str) -> Result<Self> {
        Ok(Self {
            collection: validate_collection(collection)?,
        })
    }

    pub(super) fn collection_name(&self) -> String {
        String::from_utf8_lossy(&self.collection).to_string()
    }

    pub(super) fn node_key(&self, node: CxId) -> Vec<u8> {
        let mut key = self.kind_prefix(KIND_NODE);
        key.extend_from_slice(node.as_bytes());
        key
    }

    pub(super) fn edge_out_key(&self, src: CxId, edge_type: &str, dst: CxId) -> Result<Vec<u8>> {
        validate_edge_type(edge_type)?;
        let mut key = self.kind_prefix(KIND_EDGE_OUT);
        key.extend_from_slice(src.as_bytes());
        encode_edge_type(edge_type, &mut key);
        key.extend_from_slice(dst.as_bytes());
        Ok(key)
    }

    pub(super) fn edge_in_key(&self, dst: CxId, edge_type: &str, src: CxId) -> Result<Vec<u8>> {
        validate_edge_type(edge_type)?;
        let mut key = self.kind_prefix(KIND_EDGE_IN);
        key.extend_from_slice(dst.as_bytes());
        encode_edge_type(edge_type, &mut key);
        key.extend_from_slice(src.as_bytes());
        Ok(key)
    }

    pub(super) fn csr_key(&self) -> Vec<u8> {
        self.kind_prefix(KIND_CSR)
    }

    /// Key for one byte-segment of a sharded CSR projection (#996): large
    /// graphs cannot persist as a single CF row, so the CSR row holds a
    /// manifest and the JSON bytes live in ordered segment rows.
    pub(super) fn csr_segment_key(&self, ordinal: u32) -> Vec<u8> {
        let mut key = self.kind_prefix(KIND_CSR_SEGMENT);
        key.extend_from_slice(&ordinal.to_be_bytes());
        key
    }

    pub(super) fn metadata_key(&self, name: &str) -> Result<Vec<u8>> {
        validate_metadata_name(name)?;
        let mut key = self.kind_prefix(KIND_METADATA);
        encode_edge_type(name, &mut key);
        Ok(key)
    }

    pub(super) fn metadata_range(&self) -> KeyRange {
        prefix_range(&self.kind_prefix(KIND_METADATA))
    }

    pub(super) fn collection_range(&self) -> KeyRange {
        prefix_range(&self.collection_prefix())
    }

    pub(super) fn node_range(&self) -> KeyRange {
        prefix_range(&self.kind_prefix(KIND_NODE))
    }

    pub(super) fn edge_out_range(&self) -> KeyRange {
        prefix_range(&self.kind_prefix(KIND_EDGE_OUT))
    }

    pub(super) fn csr_segment_range(&self) -> KeyRange {
        prefix_range(&self.kind_prefix(KIND_CSR_SEGMENT))
    }

    pub(super) fn edge_prefix(
        &self,
        outgoing: bool,
        node: CxId,
        edge_type: Option<&str>,
    ) -> Result<KeyRange> {
        let mut key = self.kind_prefix(if outgoing {
            KIND_EDGE_OUT
        } else {
            KIND_EDGE_IN
        });
        key.extend_from_slice(node.as_bytes());
        if let Some(edge_type) = edge_type {
            validate_edge_type(edge_type)?;
            encode_edge_type(edge_type, &mut key);
        }
        Ok(prefix_range(&key))
    }

    pub(super) fn decode_node_key(&self, key: &[u8]) -> Result<CxId> {
        let prefix = self.kind_prefix(KIND_NODE);
        if !key.starts_with(&prefix) || key.len() != prefix.len() + ID_BYTES {
            return Err(graph_corrupt("invalid graph node key"));
        }
        read_id(key, prefix.len()).ok_or_else(|| graph_corrupt("short graph node key"))
    }

    pub(super) fn decode_edge_out_key(&self, key: &[u8]) -> Result<DecodedEdge> {
        let prefix = self.kind_prefix(KIND_EDGE_OUT);
        if !key.starts_with(&prefix) {
            return Err(graph_corrupt("graph edge-out key has wrong prefix"));
        }
        let mut offset = prefix.len();
        let src = read_id(key, offset).ok_or_else(|| graph_corrupt("short graph edge src"))?;
        offset += ID_BYTES;
        let edge_type = read_edge_type(key, &mut offset)?;
        let dst = read_id(key, offset).ok_or_else(|| graph_corrupt("short graph edge dst"))?;
        ensure_consumed(key, offset + ID_BYTES)?;
        Ok(DecodedEdge {
            src,
            dst,
            edge_type,
        })
    }

    pub(super) fn decode_edge_in_key(&self, key: &[u8]) -> Result<DecodedEdge> {
        let prefix = self.kind_prefix(KIND_EDGE_IN);
        if !key.starts_with(&prefix) {
            return Err(graph_corrupt("graph edge-in key has wrong prefix"));
        }
        let mut offset = prefix.len();
        let dst = read_id(key, offset).ok_or_else(|| graph_corrupt("short graph reverse dst"))?;
        offset += ID_BYTES;
        let edge_type = read_edge_type(key, &mut offset)?;
        let src = read_id(key, offset).ok_or_else(|| graph_corrupt("short graph reverse src"))?;
        ensure_consumed(key, offset + ID_BYTES)?;
        Ok(DecodedEdge {
            src,
            dst,
            edge_type,
        })
    }

    fn collection_prefix(&self) -> Vec<u8> {
        let mut key = Vec::with_capacity(3 + self.collection.len());
        key.push(DISC);
        key.extend_from_slice(&(self.collection.len() as u16).to_be_bytes());
        key.extend_from_slice(&self.collection);
        key
    }

    fn kind_prefix(&self, kind: u8) -> Vec<u8> {
        let mut key = self.collection_prefix();
        key.push(kind);
        key
    }
}

pub(super) fn all_graph_rows_range() -> KeyRange {
    prefix_range(&[DISC])
}

pub(super) fn collection_from_key(key: &[u8]) -> Result<String> {
    if key.len() < 4 || key[0] != DISC {
        return Err(graph_corrupt("row is not a plain-graph key"));
    }
    let collection_len = u16::from_be_bytes([key[1], key[2]]) as usize;
    let collection_end = 3usize
        .checked_add(collection_len)
        .ok_or_else(|| graph_corrupt("graph collection length overflow"))?;
    let collection = key
        .get(3..collection_end)
        .ok_or_else(|| graph_corrupt("short graph collection key"))?;
    let kind = *key
        .get(collection_end)
        .ok_or_else(|| graph_corrupt("graph key omits row kind"))?;
    if kind > KIND_CSR_SEGMENT {
        return Err(graph_corrupt(format!("unknown graph row kind {kind}")));
    }
    let collection = std::str::from_utf8(collection)
        .map_err(|error| graph_corrupt(format!("invalid graph collection utf8: {error}")))?;
    GraphKeyspace::new(collection)?;
    Ok(collection.to_string())
}

pub(super) fn validate_edge_type(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_EDGE_TYPE_BYTES || bytes.iter().any(|b| *b < 0x20) {
        return Err(graph_invalid(
            "graph edge type must be printable and 1..=128 bytes",
        ));
    }
    Ok(())
}

pub(super) fn validate_value(field: &str, value: &[u8]) -> Result<()> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(graph_invalid(format!(
            "{field} exceeds {MAX_VALUE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_metadata_name(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_EDGE_TYPE_BYTES || bytes.iter().any(|b| *b < 0x20) {
        return Err(graph_invalid(
            "graph metadata name must be printable and 1..=128 bytes",
        ));
    }
    Ok(())
}

pub(super) fn graph_missing(message: impl Into<String>) -> CalyxError {
    graph_error("CALYX_GRAPH_NODE_NOT_FOUND", message)
}

pub(super) fn graph_limit(message: impl Into<String>) -> CalyxError {
    graph_error("CALYX_GRAPH_TRAVERSE_LIMIT", message)
}

pub(super) fn graph_corrupt(message: impl Into<String>) -> CalyxError {
    graph_error("CALYX_GRAPH_CORRUPT_ROW", message)
}

pub(super) fn path_error(error: impl std::fmt::Display) -> CalyxError {
    graph_corrupt(format!(
        "calyx-paths CSR projection rejected graph row: {error}"
    ))
}

fn validate_collection(value: &str) -> Result<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_COLLECTION_BYTES || bytes.iter().any(|b| *b < 0x20) {
        return Err(graph_invalid(
            "graph collection id must be printable and <=256 bytes",
        ));
    }
    Ok(bytes.to_vec())
}

fn encode_edge_type(value: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn read_edge_type(key: &[u8], offset: &mut usize) -> Result<String> {
    if key.len() < *offset + 2 {
        return Err(graph_corrupt("short graph edge-type length"));
    }
    let len = u16::from_be_bytes([key[*offset], key[*offset + 1]]) as usize;
    *offset += 2;
    if len == 0 || len > MAX_EDGE_TYPE_BYTES || key.len() < *offset + len + ID_BYTES {
        return Err(graph_corrupt("invalid graph edge-type length"));
    }
    let value = std::str::from_utf8(&key[*offset..*offset + len])
        .map_err(|error| graph_corrupt(format!("invalid graph edge-type utf8: {error}")))?;
    *offset += len;
    Ok(value.to_string())
}

fn read_id(key: &[u8], offset: usize) -> Option<CxId> {
    let bytes = key.get(offset..offset + ID_BYTES)?;
    let mut id = [0_u8; ID_BYTES];
    id.copy_from_slice(bytes);
    Some(CxId::from_bytes(id))
}

fn ensure_consumed(key: &[u8], offset: usize) -> Result<()> {
    if key.len() == offset {
        Ok(())
    } else {
        Err(graph_corrupt("graph key has trailing bytes"))
    }
}

fn graph_invalid(message: impl Into<String>) -> CalyxError {
    graph_error("CALYX_GRAPH_INVALID_KEY", message)
}

fn graph_error(code: &'static str, message: impl Into<String>) -> CalyxError {
    CalyxError {
        code,
        message: message.into(),
        remediation: "fix graph key/value input or rebuild the plain graph projection",
    }
}
