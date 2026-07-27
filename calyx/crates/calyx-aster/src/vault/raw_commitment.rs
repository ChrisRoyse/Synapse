//! Compact provenance commitments for raw (non-Ledger) Aster commits.
//!
//! A raw commit receives one fixed-size row in [`ColumnFamily::RawCommitment`]
//! in the same WAL/MVCC transaction as its data. Checkpoint maintenance then
//! seals an ordered range of those rows into the append-only Ledger. The
//! commitment CF survives checkpoint coalescing because its keys are unique by
//! commit sequence, while the Ledger receives one entry per checkpoint cohort
//! instead of one entry per high-frequency raw write.

use super::encode::WriteRow;
use crate::cf::ColumnFamily;
use calyx_core::{CalyxError, Result};
use calyx_ledger::{EntryKind, LedgerEntry, SubjectId};
use sha2::{Digest as _, Sha256};

pub(crate) const RAW_COMMITMENT_SUBJECT: &[u8] = b"calyx-aster/raw-batch-commitment/v1";

const ROW_MAGIC: &[u8; 8] = b"CYXRAW01";
const SEAL_MAGIC: &[u8; 8] = b"CYXSEAL1";
const HASH_BYTES: usize = 32;
const ROW_VALUE_BYTES: usize = ROW_MAGIC.len() + 8 + 8 + HASH_BYTES;
const SEAL_PAYLOAD_BYTES: usize = SEAL_MAGIC.len() + 8 + 8 + 8 + HASH_BYTES;
const BATCH_HASH_DOMAIN: &[u8] = b"calyx-aster/raw-batch/v1";
const MERKLE_LEAF_DOMAIN: &[u8] = b"calyx-aster/raw-commitment-leaf/v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawCommitment {
    pub seq: u64,
    pub row_count: u64,
    pub batch_hash: [u8; HASH_BYTES],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawCommitmentSeal {
    pub first_seq: u64,
    pub last_seq: u64,
    pub commitment_count: u64,
    pub merkle_root: [u8; HASH_BYTES],
}

pub(crate) fn commitment_row(seq: u64, rows: &[WriteRow]) -> Result<WriteRow> {
    if seq == 0 || rows.is_empty() {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "raw-batch commitment requires a non-zero sequence and at least one source row: seq={seq} rows={}",
            rows.len()
        )));
    }
    if let Some(row) = rows
        .iter()
        .find(|row| matches!(row.cf, ColumnFamily::Ledger | ColumnFamily::RawCommitment))
    {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "raw-batch commitment source contains reserved CF {}; Ledger-bearing commits are already provenance chained and commitment rows cannot recursively commit themselves",
            row.cf.name()
        )));
    }

    let row_count = u64::try_from(rows.len()).map_err(|_| {
        CalyxError::aster_corrupt_shard("raw-batch row count does not fit the durable u64 codec")
    })?;
    let mut hasher = Sha256::new();
    hasher.update(BATCH_HASH_DOMAIN);
    hasher.update(seq.to_be_bytes());
    hasher.update(row_count.to_be_bytes());
    for (ordinal, row) in rows.iter().enumerate() {
        let ordinal = u64::try_from(ordinal).map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "raw-batch row ordinal does not fit the durable u64 codec",
            )
        })?;
        let cf_tag = row.cf.keyspace_tag();
        hasher.update(ordinal.to_be_bytes());
        hash_len_prefixed(&mut hasher, &cf_tag)?;
        hash_len_prefixed(&mut hasher, &row.key)?;
        hash_len_prefixed(&mut hasher, &row.value)?;
    }
    let commitment = RawCommitment {
        seq,
        row_count,
        batch_hash: hasher.finalize().into(),
    };
    Ok(WriteRow {
        cf: ColumnFamily::RawCommitment,
        key: seq.to_be_bytes().to_vec(),
        value: encode_commitment(&commitment),
    })
}

pub(crate) fn decode_commitment(key: &[u8], value: &[u8]) -> Result<RawCommitment> {
    if key.len() != 8 || value.len() != ROW_VALUE_BYTES || &value[..ROW_MAGIC.len()] != ROW_MAGIC {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "malformed raw-batch commitment row: key_bytes={} value_bytes={} expected_key_bytes=8 expected_value_bytes={ROW_VALUE_BYTES}",
            key.len(),
            value.len()
        )));
    }
    let key_seq = u64::from_be_bytes(key.try_into().map_err(|_| {
        CalyxError::aster_corrupt_shard("raw-batch commitment key length changed after validation")
    })?);
    let seq = read_u64(value, ROW_MAGIC.len())?;
    let row_count = read_u64(value, ROW_MAGIC.len() + 8)?;
    let batch_hash = read_hash(value, ROW_MAGIC.len() + 16)?;
    if key_seq == 0 || seq == 0 || key_seq != seq || row_count == 0 {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "invalid raw-batch commitment identity: key_seq={key_seq} value_seq={seq} row_count={row_count}"
        )));
    }
    Ok(RawCommitment {
        seq,
        row_count,
        batch_hash,
    })
}

pub(crate) fn seal(commitments: &[RawCommitment]) -> Result<RawCommitmentSeal> {
    let first = commitments.first().ok_or_else(|| {
        CalyxError::aster_corrupt_shard("cannot seal an empty raw-batch commitment cohort")
    })?;
    if let Some(window) = commitments
        .windows(2)
        .find(|window| window[0].seq >= window[1].seq)
    {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "raw-batch commitments are not strictly sequence ordered: {} then {}",
            window[0].seq, window[1].seq
        )));
    }
    let last = commitments.last().expect("non-empty cohort has a last row");
    let commitment_count = u64::try_from(commitments.len()).map_err(|_| {
        CalyxError::aster_corrupt_shard(
            "raw-batch commitment cohort length does not fit the durable u64 codec",
        )
    })?;
    let leaves = commitments
        .iter()
        .map(commitment_leaf_hash)
        .collect::<Vec<_>>();
    Ok(RawCommitmentSeal {
        first_seq: first.seq,
        last_seq: last.seq,
        commitment_count,
        merkle_root: merkle_root(&leaves),
    })
}

pub(crate) fn encode_seal(seal: &RawCommitmentSeal) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SEAL_PAYLOAD_BYTES);
    bytes.extend_from_slice(SEAL_MAGIC);
    bytes.extend_from_slice(&seal.first_seq.to_be_bytes());
    bytes.extend_from_slice(&seal.last_seq.to_be_bytes());
    bytes.extend_from_slice(&seal.commitment_count.to_be_bytes());
    bytes.extend_from_slice(&seal.merkle_root);
    bytes
}

pub(crate) fn decode_seal(payload: &[u8]) -> Result<RawCommitmentSeal> {
    if payload.len() != SEAL_PAYLOAD_BYTES || &payload[..SEAL_MAGIC.len()] != SEAL_MAGIC {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "malformed raw-batch Ledger seal: payload_bytes={} expected={SEAL_PAYLOAD_BYTES}",
            payload.len()
        )));
    }
    let seal = RawCommitmentSeal {
        first_seq: read_u64(payload, SEAL_MAGIC.len())?,
        last_seq: read_u64(payload, SEAL_MAGIC.len() + 8)?,
        commitment_count: read_u64(payload, SEAL_MAGIC.len() + 16)?,
        merkle_root: read_hash(payload, SEAL_MAGIC.len() + 24)?,
    };
    if seal.first_seq == 0
        || seal.last_seq < seal.first_seq
        || seal.commitment_count == 0
        || seal.commitment_count
            > seal
                .last_seq
                .saturating_sub(seal.first_seq)
                .saturating_add(1)
    {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "invalid raw-batch Ledger seal range: first_seq={} last_seq={} commitment_count={}",
            seal.first_seq, seal.last_seq, seal.commitment_count
        )));
    }
    Ok(seal)
}

pub(crate) fn ledger_seal(entry: &LedgerEntry) -> Result<Option<RawCommitmentSeal>> {
    if entry.kind != EntryKind::BatchCommitment {
        return Ok(None);
    }
    if entry.subject != SubjectId::Query(RAW_COMMITMENT_SUBJECT.to_vec()) {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "batch_commitment Ledger entry {} has a foreign subject; the kind is reserved for Aster raw-batch checkpoint seals",
            entry.seq
        )));
    }
    decode_seal(&entry.payload).map(Some)
}

pub(crate) fn seal_matches(
    seal: &RawCommitmentSeal,
    commitments: &[RawCommitment],
) -> Result<bool> {
    if commitments.is_empty() {
        return Ok(false);
    }
    let expected = self::seal(commitments)?;
    Ok(&expected == seal)
}

fn encode_commitment(commitment: &RawCommitment) -> Vec<u8> {
    let mut value = Vec::with_capacity(ROW_VALUE_BYTES);
    value.extend_from_slice(ROW_MAGIC);
    value.extend_from_slice(&commitment.seq.to_be_bytes());
    value.extend_from_slice(&commitment.row_count.to_be_bytes());
    value.extend_from_slice(&commitment.batch_hash);
    value
}

fn commitment_leaf_hash(commitment: &RawCommitment) -> [u8; HASH_BYTES] {
    let key = commitment.seq.to_be_bytes();
    let value = encode_commitment(commitment);
    let mut hasher = Sha256::new();
    hasher.update([0]);
    hasher.update(MERKLE_LEAF_DOMAIN);
    hasher.update((key.len() as u64).to_be_bytes());
    hasher.update(key);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
    hasher.finalize().into()
}

fn merkle_root(leaves: &[[u8; HASH_BYTES]]) -> [u8; HASH_BYTES] {
    match leaves.len() {
        0 => Sha256::digest([]).into(),
        1 => leaves[0],
        len => {
            let split = len.next_power_of_two() / 2;
            let left = merkle_root(&leaves[..split]);
            let right = merkle_root(&leaves[split..]);
            let mut hasher = Sha256::new();
            hasher.update([1]);
            hasher.update(left);
            hasher.update(right);
            hasher.finalize().into()
        }
    }
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) -> Result<()> {
    let len = u64::try_from(bytes.len()).map_err(|_| {
        CalyxError::aster_corrupt_shard(
            "raw-batch commitment field length does not fit the durable u64 codec",
        )
    })?;
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
    Ok(())
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes.get(offset..offset + 8).ok_or_else(|| {
        CalyxError::aster_corrupt_shard(format!(
            "raw-batch commitment u64 field truncated at offset {offset}"
        ))
    })?;
    Ok(u64::from_be_bytes(slice.try_into().map_err(|_| {
        CalyxError::aster_corrupt_shard(
            "raw-batch commitment u64 field length changed after validation",
        )
    })?))
}

fn read_hash(bytes: &[u8], offset: usize) -> Result<[u8; HASH_BYTES]> {
    bytes
        .get(offset..offset + HASH_BYTES)
        .ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "raw-batch commitment hash field truncated at offset {offset}"
            ))
        })?
        .try_into()
        .map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "raw-batch commitment hash length changed after validation",
            )
        })
}
