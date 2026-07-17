use crate::cf::ColumnFamily;
use calyx_core::{
    AbsentReason, CalyxError, Constellation, CxFlags, CxId, InputRef, LedgerRef, Modality, Result,
    SlotId, SlotVector, SparseEntry, VaultId,
};
use std::collections::BTreeMap;

mod components;

pub use super::anchor_codec::{decode_anchor, encode_anchor};
use super::cf_codec::{cf_tag, decode_cf};
use super::cursor::Cursor;
use components::*;

pub const HEADER_LEN: usize = 102;
const IDENTITY_HASH_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstellationHeader {
    pub cx_id: CxId,
    pub vault_id: VaultId,
    pub panel_version: u32,
    pub created_at: u64,
    pub modality: Modality,
    pub flags: CxFlags,
    pub n_slots: u16,
    pub n_anchors: u16,
    pub ledger_seq: u64,
    pub input_hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteRow {
    pub cf: ColumnFamily,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncodedWriteRow<'a> {
    pub(crate) cf: ColumnFamily,
    pub(crate) key: &'a [u8],
    pub(crate) value: &'a [u8],
    /// Offset of the CF tag from the start of the encoded write-batch payload.
    pub(crate) encoded_offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodedSlotVectorShape {
    Dense { dim: u32 },
    Absent,
    Sparse { dim: u32, entry_count: u32 },
    Multi { token_dim: u32, token_count: u32 },
}

/// A borrowed, allocation-free view of an encoded multi-vector slot payload.
///
/// Aster stores multi-vector components as contiguous big-endian `f32` bits.
/// Consumers that only need to stream those components should use this view
/// instead of expanding the payload into `Vec<Vec<f32>>`.
#[derive(Clone, Copy, Debug)]
pub struct EncodedMultiSlotVector<'a> {
    token_dim: u32,
    token_count: u32,
    component_bytes: &'a [u8],
}

impl<'a> EncodedMultiSlotVector<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self> {
        let EncodedSlotVectorShape::Multi {
            token_dim,
            token_count,
        } = inspect_slot_vector(bytes)?
        else {
            return Err(CalyxError::aster_corrupt_shard(
                "encoded slot vector is not a multi-vector payload",
            ));
        };
        Ok(Self {
            token_dim,
            token_count,
            component_bytes: &bytes[9..],
        })
    }

    pub const fn token_dim(self) -> u32 {
        self.token_dim
    }

    pub const fn token_count(self) -> u32 {
        self.token_count
    }

    pub fn components(self) -> impl ExactSizeIterator<Item = f32> + 'a {
        self.component_bytes.chunks_exact(4).map(|bytes| {
            f32::from_bits(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        })
    }
}

pub fn encode_header(cx: &Constellation) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(cx.cx_id.as_bytes());
    out.extend_from_slice(&cx.vault_id.as_ulid().to_bytes());
    out.extend_from_slice(&cx.panel_version.to_be_bytes());
    out.extend_from_slice(&cx.created_at.to_be_bytes());
    out.push(modality_tag(cx.modality));
    out.push(flags_bits(cx.flags));
    out.extend_from_slice(&(cx.slots.len() as u16).to_be_bytes());
    out.extend_from_slice(&(cx.anchors.len() as u16).to_be_bytes());
    out.extend_from_slice(&cx.provenance.seq.to_be_bytes());
    out.extend_from_slice(&cx.input_ref.hash);
    out.extend_from_slice(&[0_u8; 12]);
    out
}

pub fn decode_header(bytes: &[u8]) -> Result<ConstellationHeader> {
    if bytes.len() < HEADER_LEN {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "constellation header too short: {} < {HEADER_LEN}",
            bytes.len()
        )));
    }
    let mut cursor = Cursor::new(bytes);
    let cx_id = CxId::from_bytes(cursor.array()?);
    let vault_id = VaultId::from_ulid(ulid::Ulid::from_bytes(cursor.array()?));
    let panel_version = cursor.u32()?;
    let created_at = cursor.u64()?;
    let modality = decode_modality(cursor.u8()?)?;
    let flags = decode_flags(cursor.u8()?);
    let n_slots = cursor.u16()?;
    let n_anchors = cursor.u16()?;
    let ledger_seq = cursor.u64()?;
    let input_hash = cursor.array()?;
    Ok(ConstellationHeader {
        cx_id,
        vault_id,
        panel_version,
        created_at,
        modality,
        flags,
        n_slots,
        n_anchors,
        ledger_seq,
        input_hash,
    })
}

pub fn encode_constellation_base(cx: &Constellation) -> Result<Vec<u8>> {
    let slot_hashes = cx
        .slots
        .iter()
        .map(|(slot, vector)| {
            let bytes = encode_slot_vector(vector)?;
            Ok((*slot, hash_slot_bytes(&bytes)))
        })
        .collect::<Result<Vec<_>>>()?;
    encode_constellation_base_with_slot_hashes(cx, &slot_hashes)
}

pub(super) fn encode_constellation_base_with_slot_hashes(
    cx: &Constellation,
    slot_hashes: &[(SlotId, [u8; 32])],
) -> Result<Vec<u8>> {
    validate_slot_hashes(cx, slot_hashes)?;
    let mut out = encode_header(cx);
    out.extend_from_slice(identity_hash_with_slot_hashes(cx, slot_hashes)?.as_bytes());
    encode_input_ref_tail(&cx.input_ref, &mut out)?;
    out.extend_from_slice(&(cx.slots.len() as u16).to_be_bytes());
    for (slot, hash) in slot_hashes {
        out.extend_from_slice(&slot.get().to_be_bytes());
        out.extend_from_slice(hash);
    }
    out.extend_from_slice(&(cx.scalars.len() as u32).to_be_bytes());
    for (key, value) in &cx.scalars {
        put_string(&mut out, key)?;
        out.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    out.extend_from_slice(&(cx.anchors.len() as u32).to_be_bytes());
    for anchor in &cx.anchors {
        put_bytes(&mut out, &encode_anchor(anchor)?)?;
    }
    out.extend_from_slice(&cx.provenance.hash);
    encode_string_metadata(&cx.metadata, &mut out)?;
    Ok(out)
}

pub fn decode_constellation_base(bytes: &[u8]) -> Result<Constellation> {
    let header = decode_header(bytes)?;
    let mut cursor = Cursor::new(&bytes[HEADER_LEN..]);
    let _identity = cursor.bytes(IDENTITY_HASH_LEN)?;
    let input_ref = decode_input_ref_tail(&mut cursor, header.input_hash)?;
    let slot_count = cursor.u16()? as usize;
    let mut slots = BTreeMap::new();
    for _ in 0..slot_count {
        let slot = SlotId::new(cursor.u16()?);
        let _hash = cursor.bytes(IDENTITY_HASH_LEN)?;
        slots.insert(
            slot,
            SlotVector::Absent {
                reason: AbsentReason::NotApplicable,
            },
        );
    }
    let scalar_count = cursor.u32()? as usize;
    let mut scalars = BTreeMap::new();
    for _ in 0..scalar_count {
        let key = cursor.string()?;
        scalars.insert(key, f64::from_bits(cursor.u64()?));
    }
    let anchor_count = cursor.u32()? as usize;
    let mut anchors = Vec::with_capacity(anchor_count);
    for _ in 0..anchor_count {
        anchors.push(decode_anchor(cursor.bytes_prefixed()?)?);
    }
    let provenance = LedgerRef {
        seq: header.ledger_seq,
        hash: cursor.array()?,
    };
    let metadata = if cursor.remaining() == 0 {
        BTreeMap::new()
    } else {
        decode_string_metadata(&mut cursor)?
    };
    if cursor.remaining() != 0 {
        return Err(CalyxError::aster_corrupt_shard(
            "trailing bytes after constellation metadata",
        ));
    }
    Ok(Constellation {
        cx_id: header.cx_id,
        vault_id: header.vault_id,
        panel_version: header.panel_version,
        created_at: header.created_at,
        input_ref,
        modality: header.modality,
        slots,
        scalars,
        metadata,
        anchors,
        provenance,
        flags: header.flags,
    })
}

pub fn same_constellation_identity(left: &[u8], right: &[u8]) -> Result<bool> {
    Ok(decode_identity(left)? == decode_identity(right)?)
}

pub fn encode_slot_vector(vector: &SlotVector) -> Result<Vec<u8>> {
    vector.validate_schema().map_err(|error| {
        CalyxError::aster_corrupt_shard(format!(
            "cannot encode invalid slot vector: {}",
            error.message
        ))
    })?;
    let mut out = Vec::new();
    match vector {
        SlotVector::Dense { dim, data } => {
            if *dim as usize != data.len() {
                return Err(CalyxError::aster_corrupt_shard(
                    "dense slot dim does not match data length",
                ));
            }
            out.push(0);
            out.extend_from_slice(&dim.to_be_bytes());
            for value in data {
                out.extend_from_slice(&value.to_bits().to_be_bytes());
            }
        }
        SlotVector::Absent { reason } => {
            out.push(1);
            encode_absent_reason(reason, &mut out)?;
        }
        SlotVector::Sparse { dim, entries } => {
            out.push(2);
            out.extend_from_slice(&dim.to_be_bytes());
            let entry_count = u32::try_from(entries.len()).map_err(|_| {
                CalyxError::aster_corrupt_shard("sparse slot has more than u32::MAX entries")
            })?;
            out.extend_from_slice(&entry_count.to_be_bytes());
            for entry in entries {
                out.extend_from_slice(&entry.idx.to_be_bytes());
                out.extend_from_slice(&entry.val.to_bits().to_be_bytes());
            }
        }
        SlotVector::Multi { token_dim, tokens } => {
            out.push(3);
            out.extend_from_slice(&token_dim.to_be_bytes());
            let token_count = u32::try_from(tokens.len()).map_err(|_| {
                CalyxError::aster_corrupt_shard("multi slot has more than u32::MAX tokens")
            })?;
            out.extend_from_slice(&token_count.to_be_bytes());
            for token in tokens {
                if token.len() != *token_dim as usize {
                    return Err(CalyxError::aster_corrupt_shard(
                        "multi slot token dim does not match token length",
                    ));
                }
                for value in token {
                    out.extend_from_slice(&value.to_bits().to_be_bytes());
                }
            }
        }
    }
    Ok(out)
}

pub fn inspect_slot_vector(bytes: &[u8]) -> Result<EncodedSlotVectorShape> {
    let mut cursor = Cursor::new(bytes);
    let shape = match cursor.u8()? {
        0 => {
            let dim = cursor.u32()?;
            require_nonzero_slot_count("dense dim", dim)?;
            require_slot_vector_len(
                "dense",
                bytes.len(),
                checked_encoded_len(5, dim as usize, 4, "dense component")?,
                format!("dim={dim}"),
            )?;
            EncodedSlotVectorShape::Dense { dim }
        }
        1 => {
            let reason = cursor.u8()?;
            match reason {
                0..=4 => require_slot_vector_len(
                    "absent",
                    bytes.len(),
                    2,
                    format!("reason_tag={reason}"),
                )?,
                5 => {
                    let message_len = cursor.u32()? as usize;
                    let message = cursor.bytes(message_len)?;
                    std::str::from_utf8(message).map_err(|error| {
                        CalyxError::aster_corrupt_shard(format!(
                            "absent slot error message is not UTF-8: {error}"
                        ))
                    })?;
                    require_slot_vector_len(
                        "absent",
                        bytes.len(),
                        checked_encoded_len(6, message_len, 1, "absent error message")?,
                        format!("reason_tag={reason}, message_len={message_len}"),
                    )?;
                }
                tag => {
                    return Err(CalyxError::aster_corrupt_shard(format!(
                        "unknown absent tag {tag}"
                    )));
                }
            }
            EncodedSlotVectorShape::Absent
        }
        2 => {
            let dim = cursor.u32()?;
            require_nonzero_slot_count("sparse dim", dim)?;
            let entry_count = cursor.u32()?;
            require_slot_vector_len(
                "sparse",
                bytes.len(),
                checked_encoded_len(9, entry_count as usize, 8, "sparse entry")?,
                format!("dim={dim}, entry_count={entry_count}"),
            )?;
            EncodedSlotVectorShape::Sparse { dim, entry_count }
        }
        3 => {
            let token_dim = cursor.u32()?;
            let token_count = cursor.u32()?;
            require_nonzero_slot_count("multi token_dim", token_dim)?;
            require_nonzero_slot_count("multi token_count", token_count)?;
            let component_count = (token_count as usize)
                .checked_mul(token_dim as usize)
                .ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "multi slot component count overflow: token_count={token_count}, token_dim={token_dim}"
                    ))
                })?;
            require_slot_vector_len(
                "multi",
                bytes.len(),
                checked_encoded_len(9, component_count, 4, "multi component")?,
                format!("token_count={token_count}, token_dim={token_dim}"),
            )?;
            EncodedSlotVectorShape::Multi {
                token_dim,
                token_count,
            }
        }
        tag => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "unknown slot vector tag {tag}"
            )));
        }
    };
    Ok(shape)
}

pub fn decode_slot_vector(bytes: &[u8]) -> Result<SlotVector> {
    let shape = inspect_slot_vector(bytes)?;
    let mut cursor = Cursor::new(bytes);
    let _tag = cursor.u8()?;
    let vector = match shape {
        EncodedSlotVectorShape::Dense { dim } => {
            let encoded_dim = cursor.u32()?;
            debug_assert_eq!(encoded_dim, dim);
            let mut data = reserved_vec(dim as usize, "dense slot components")?;
            for _ in 0..dim {
                data.push(f32::from_bits(cursor.u32()?));
            }
            SlotVector::Dense { dim, data }
        }
        EncodedSlotVectorShape::Absent => SlotVector::Absent {
            reason: decode_absent_reason(&mut cursor)?,
        },
        EncodedSlotVectorShape::Sparse { dim, entry_count } => {
            let encoded_dim = cursor.u32()?;
            let encoded_count = cursor.u32()?;
            debug_assert_eq!(encoded_dim, dim);
            debug_assert_eq!(encoded_count, entry_count);
            let mut entries = reserved_vec(entry_count as usize, "sparse slot entries")?;
            for _ in 0..entry_count {
                entries.push(SparseEntry {
                    idx: cursor.u32()?,
                    val: f32::from_bits(cursor.u32()?),
                });
            }
            SlotVector::Sparse { dim, entries }
        }
        EncodedSlotVectorShape::Multi {
            token_dim,
            token_count,
        } => {
            let encoded_dim = cursor.u32()?;
            let encoded_count = cursor.u32()?;
            debug_assert_eq!(encoded_dim, token_dim);
            debug_assert_eq!(encoded_count, token_count);
            let mut tokens = reserved_vec(token_count as usize, "multi slot tokens")?;
            for _ in 0..token_count {
                let mut token = reserved_vec(token_dim as usize, "multi slot token components")?;
                for _ in 0..token_dim {
                    token.push(f32::from_bits(cursor.u32()?));
                }
                tokens.push(token);
            }
            SlotVector::Multi { token_dim, tokens }
        }
    };
    vector.validate_schema().map_err(|error| {
        CalyxError::aster_corrupt_shard(format!(
            "decoded slot vector violates schema: {}",
            error.message
        ))
    })?;
    Ok(vector)
}

pub fn encode_write_batch(rows: &[WriteRow]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(&(rows.len() as u32).to_be_bytes());
    for row in rows {
        out.push(cf_tag(row.cf)?);
        put_bytes(&mut out, &row.key)?;
        put_bytes(&mut out, &row.value)?;
    }
    Ok(out)
}

pub fn decode_write_batch(bytes: &[u8]) -> Result<Vec<WriteRow>> {
    Ok(decode_write_batch_refs(bytes)?
        .into_iter()
        .map(|row| WriteRow {
            cf: row.cf,
            key: row.key.to_vec(),
            value: row.value.to_vec(),
        })
        .collect())
}

pub(crate) fn decode_write_batch_refs(bytes: &[u8]) -> Result<Vec<EncodedWriteRow<'_>>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.u32()? as usize;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let encoded_offset = cursor.position();
        rows.push(EncodedWriteRow {
            cf: decode_cf(cursor.u8()?)?,
            key: cursor.bytes_prefixed()?,
            value: cursor.bytes_prefixed()?,
            encoded_offset,
        });
    }
    if cursor.remaining() != 0 {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "trailing bytes after encoded write batch: {}",
            cursor.remaining()
        )));
    }
    Ok(rows)
}

fn decode_identity(bytes: &[u8]) -> Result<(ConstellationHeader, [u8; 32])> {
    let header = decode_header(bytes)?;
    let mut cursor = Cursor::new(&bytes[HEADER_LEN..]);
    let identity = cursor.array()?;
    Ok((header_without_anchor_count(header), identity))
}

fn header_without_anchor_count(mut header: ConstellationHeader) -> ConstellationHeader {
    header.n_anchors = 0;
    header
}

fn identity_hash_with_slot_hashes(
    cx: &Constellation,
    slot_hashes: &[(SlotId, [u8; 32])],
) -> Result<blake3::Hash> {
    let mut bytes = encode_header(cx);
    bytes[50..58].copy_from_slice(&0_u64.to_be_bytes());
    bytes[48..50].copy_from_slice(&0_u16.to_be_bytes());
    for (slot, hash) in slot_hashes {
        bytes.extend_from_slice(&slot.get().to_be_bytes());
        bytes.extend_from_slice(hash);
    }
    for (key, value) in &cx.scalars {
        put_string(&mut bytes, key)?;
        bytes.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    if !cx.metadata.is_empty() {
        encode_string_metadata(&cx.metadata, &mut bytes)?;
    }
    bytes.extend_from_slice(&[0_u8; 32]);
    Ok(blake3::hash(&bytes))
}

fn validate_slot_hashes(cx: &Constellation, slot_hashes: &[(SlotId, [u8; 32])]) -> Result<()> {
    if slot_hashes.len() != cx.slots.len()
        || !cx
            .slots
            .keys()
            .copied()
            .eq(slot_hashes.iter().map(|(slot, _)| *slot))
    {
        return Err(CalyxError::aster_corrupt_shard(
            "prepared slot hashes do not match constellation slot order",
        ));
    }
    Ok(())
}

pub(super) fn hash_slot_bytes(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}
