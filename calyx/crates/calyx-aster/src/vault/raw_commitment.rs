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
/// Seal payload carrying the bounded localization ladder (issue #1876).
const SEAL_MAGIC_V2: &[u8; 8] = b"CYXSEAL2";
const HASH_BYTES: usize = 32;
const ROW_VALUE_BYTES: usize = ROW_MAGIC.len() + 8 + 8 + HASH_BYTES;
const SEAL_PAYLOAD_BYTES: usize = SEAL_MAGIC.len() + 8 + 8 + 8 + HASH_BYTES;
const SEAL_V2_HEADER_BYTES: usize = SEAL_MAGIC_V2.len() + 8 + 8 + 8 + HASH_BYTES + 4;
const BATCH_HASH_DOMAIN: &[u8] = b"calyx-aster/raw-batch/v1";
const MERKLE_LEAF_DOMAIN: &[u8] = b"calyx-aster/raw-commitment-leaf/v1";

/// Upper bound on the localization ladder stored beside the Merkle root.
///
/// A Merkle root alone cannot say *which* leaf moved: RFC 6962 needs an
/// inclusion proof (the sibling hashes) to say anything about an individual
/// leaf, and this seal stores only the root. Issue #1876 asked for the exact
/// offending sequence on a cohort mismatch, so the seal now also carries one
/// digest per contiguous bucket of the cohort.
///
/// A cohort of `n <= LOCALIZATION_MAX_DIGESTS` rows gets one digest per row, so
/// localization is exact. Larger cohorts are bucketed, which keeps the payload
/// bounded at 8 KiB and still narrows a failure to `ceil(n / 256)` rows instead
/// of the whole cohort. Nothing bounds a cohort's size, so the payload must be
/// bounded here rather than trusting cohorts to stay small.
const LOCALIZATION_MAX_DIGESTS: usize = 256;

/// Number of physical rows named in a diagnostic before the listing is elided.
const MISMATCH_ROW_LISTING_CAP: usize = 16;

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
    /// Bounded per-bucket digest ladder used only to localize a mismatch
    /// (issue #1876). Empty for cohorts sealed by a pre-#1876 build, which
    /// wrote the root alone. This field is **never** part of the verification
    /// verdict — see [`seal_matches`].
    pub leaf_digests: Vec<[u8; HASH_BYTES]>,
}

impl RawCommitmentSeal {
    /// The four fields that decide `intact`. Detection compares exactly these,
    /// so a v1 seal and a v2 seal over the same cohort verify identically.
    fn core(&self) -> (u64, u64, u64, [u8; HASH_BYTES]) {
        (
            self.first_seq,
            self.last_seq,
            self.commitment_count,
            self.merkle_root,
        )
    }
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
        leaf_digests: localization_ladder(&leaves),
    })
}

/// Splits `leaves` into `min(len, LOCALIZATION_MAX_DIGESTS)` contiguous buckets
/// and digests each one.
///
/// The bucket digest is [`merkle_root`] of the bucket's leaves, so a bucket
/// holding exactly one leaf digests to that leaf's own hash. That makes the
/// small-cohort case — every real cohort on the production vault — exact
/// localization with no special case.
fn localization_ladder(leaves: &[[u8; HASH_BYTES]]) -> Vec<[u8; HASH_BYTES]> {
    let len = leaves.len();
    if len == 0 {
        return Vec::new();
    }
    let buckets = len.min(LOCALIZATION_MAX_DIGESTS);
    (0..buckets)
        .map(|bucket| {
            let (start, end) = localization_bucket_bounds(len, buckets, bucket);
            merkle_root(&leaves[start..end])
        })
        .collect()
}

/// Half-open `[start, end)` leaf range covered by `bucket` when `len` leaves are
/// split into `buckets` near-equal contiguous groups.
fn localization_bucket_bounds(len: usize, buckets: usize, bucket: usize) -> (usize, usize) {
    let start = bucket.saturating_mul(len) / buckets;
    let end = bucket.saturating_add(1).saturating_mul(len) / buckets;
    (start, end.max(start))
}

pub(crate) fn encode_seal(seal: &RawCommitmentSeal) -> Vec<u8> {
    if seal.leaf_digests.is_empty() {
        let mut bytes = Vec::with_capacity(SEAL_PAYLOAD_BYTES);
        bytes.extend_from_slice(SEAL_MAGIC);
        bytes.extend_from_slice(&seal.first_seq.to_be_bytes());
        bytes.extend_from_slice(&seal.last_seq.to_be_bytes());
        bytes.extend_from_slice(&seal.commitment_count.to_be_bytes());
        bytes.extend_from_slice(&seal.merkle_root);
        return bytes;
    }
    // v2: identical header fields, then the bounded localization ladder. The
    // four fields above are byte-identical to v1 and remain the whole of the
    // verification verdict; the ladder is diagnostic only (#1876).
    let digest_count = u32::try_from(seal.leaf_digests.len()).unwrap_or(u32::MAX);
    let mut bytes = Vec::with_capacity(SEAL_V2_HEADER_BYTES + seal.leaf_digests.len() * HASH_BYTES);
    bytes.extend_from_slice(SEAL_MAGIC_V2);
    bytes.extend_from_slice(&seal.first_seq.to_be_bytes());
    bytes.extend_from_slice(&seal.last_seq.to_be_bytes());
    bytes.extend_from_slice(&seal.commitment_count.to_be_bytes());
    bytes.extend_from_slice(&seal.merkle_root);
    bytes.extend_from_slice(&digest_count.to_be_bytes());
    for digest in &seal.leaf_digests {
        bytes.extend_from_slice(digest);
    }
    bytes
}

pub(crate) fn decode_seal(payload: &[u8]) -> Result<RawCommitmentSeal> {
    let is_v1 = payload.len() >= SEAL_MAGIC.len() && &payload[..SEAL_MAGIC.len()] == SEAL_MAGIC;
    let is_v2 =
        payload.len() >= SEAL_MAGIC_V2.len() && &payload[..SEAL_MAGIC_V2.len()] == SEAL_MAGIC_V2;
    if !is_v1 && !is_v2 {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "malformed raw-batch Ledger seal: payload_bytes={} carries neither the v1 nor the v2 seal magic",
            payload.len()
        )));
    }
    if is_v1 && payload.len() != SEAL_PAYLOAD_BYTES {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "malformed raw-batch Ledger seal: payload_bytes={} expected={SEAL_PAYLOAD_BYTES}",
            payload.len()
        )));
    }

    let mut seal = RawCommitmentSeal {
        first_seq: read_u64(payload, SEAL_MAGIC.len())?,
        last_seq: read_u64(payload, SEAL_MAGIC.len() + 8)?,
        commitment_count: read_u64(payload, SEAL_MAGIC.len() + 16)?,
        merkle_root: read_hash(payload, SEAL_MAGIC.len() + 24)?,
        leaf_digests: Vec::new(),
    };

    if is_v2 {
        let count_offset = SEAL_MAGIC_V2.len() + 24 + HASH_BYTES;
        let count_bytes = payload.get(count_offset..count_offset + 4).ok_or_else(|| {
            CalyxError::aster_corrupt_shard(
                "raw-batch Ledger seal v2 is truncated before its digest count",
            )
        })?;
        let digest_count = u32::from_be_bytes(count_bytes.try_into().map_err(|_| {
            CalyxError::aster_corrupt_shard(
                "raw-batch Ledger seal v2 digest count length changed after validation",
            )
        })?) as usize;
        let expected_bytes = SEAL_V2_HEADER_BYTES
            .checked_add(digest_count.saturating_mul(HASH_BYTES))
            .ok_or_else(|| {
                CalyxError::aster_corrupt_shard(
                    "raw-batch Ledger seal v2 digest count overflows this host",
                )
            })?;
        if payload.len() != expected_bytes {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "malformed raw-batch Ledger seal v2: payload_bytes={} digest_count={digest_count} expected={expected_bytes}",
                payload.len()
            )));
        }
        if digest_count > LOCALIZATION_MAX_DIGESTS {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "raw-batch Ledger seal v2 carries {digest_count} localization digests, above the {LOCALIZATION_MAX_DIGESTS} bound"
            )));
        }
        let mut digests = Vec::with_capacity(digest_count);
        for index in 0..digest_count {
            digests.push(read_hash(
                payload,
                SEAL_V2_HEADER_BYTES + index * HASH_BYTES,
            )?);
        }
        seal.leaf_digests = digests;
    }

    if is_v2 {
        let expected_digests = usize::try_from(seal.commitment_count)
            .unwrap_or(usize::MAX)
            .min(LOCALIZATION_MAX_DIGESTS);
        if seal.leaf_digests.len() != expected_digests {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "raw-batch Ledger seal v2 carries {} localization digests but its cohort of {} rows requires {expected_digests}",
                seal.leaf_digests.len(),
                seal.commitment_count
            )));
        }
    }

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
    // Compare the four sealed fields only. The localization ladder is a
    // diagnostic added in #1876 and must never change a verdict, or a cohort
    // sealed by a pre-#1876 build would start failing verification.
    Ok(expected.core() == seal.core())
}

/// Explains *precisely* how a cohort diverged from its seal, on the failure
/// path only.
///
/// Issue #1876: the previous message named the sealed cohort range and nothing
/// else, so an operator facing a `CALYX_ASTER_CORRUPT_SHARD` verdict got a range
/// to hand-search rather than a sequence to look at. That vagueness is what
/// pushes an operator toward a destructive repair — the #1875 lesson.
///
/// What can be said depends on what the seal carries:
///
/// * **Identity divergence** (a row added, dropped, reordered, or shifted in
///   from outside the sealed range) is localizable from any seal, v1 or v2,
///   because the sealed `first_seq`/`last_seq`/`commitment_count` name the
///   cohort that *should* be here. These are reported as exact sequences.
/// * **Content divergence** (the row set and order are right but a row's bytes
///   changed) is localizable only against a per-leaf digest. A Merkle root alone
///   cannot do it — RFC 6962 needs an inclusion proof, i.e. the sibling hashes,
///   to say anything about an individual leaf, and this seal stores only the
///   root. v2 seals therefore carry a bounded digest ladder; v1 seals say
///   plainly that they cannot localize rather than guessing.
pub(crate) fn describe_mismatch(seal: &RawCommitmentSeal, cohort: &[RawCommitment]) -> String {
    if cohort.is_empty() {
        return format!(
            "sealed rows {}..={} count={} but the physical commitment cohort at this cursor is empty",
            seal.first_seq, seal.last_seq, seal.commitment_count
        );
    }

    let physical_first = cohort[0].seq;
    let physical_last = cohort[cohort.len() - 1].seq;
    let physical_count = cohort.len() as u64;
    let mut parts: Vec<String> = Vec::new();

    if seal.first_seq != physical_first {
        parts.push(format!(
            "first_seq sealed={} physical={}",
            seal.first_seq, physical_first
        ));
    }
    if seal.last_seq != physical_last {
        parts.push(format!(
            "last_seq sealed={} physical={}",
            seal.last_seq, physical_last
        ));
    }
    if seal.commitment_count != physical_count {
        parts.push(format!(
            "commitment_count sealed={} physical={}",
            seal.commitment_count, physical_count
        ));
    }

    // Rows sitting inside this cohort that the seal never covered are named
    // exactly, whatever the seal version.
    let intruders: Vec<u64> = cohort
        .iter()
        .map(|commitment| commitment.seq)
        .filter(|seq| *seq < seal.first_seq || *seq > seal.last_seq)
        .collect();
    if !intruders.is_empty() {
        parts.push(format!(
            "offending_sequences_outside_sealed_range=[{}]",
            join_seqs(&intruders)
        ));
    }

    let leaves: Vec<[u8; HASH_BYTES]> = cohort.iter().map(commitment_leaf_hash).collect();
    let physical_root = merkle_root(&leaves);
    if physical_root != seal.merkle_root {
        parts.push(format!(
            "merkle_root sealed={} physical={}",
            hex32(&seal.merkle_root),
            hex32(&physical_root)
        ));
        parts.push(localize_content_divergence(seal, cohort, &leaves));
    }

    if parts.is_empty() {
        // Reached only if a future field is added to the verdict without being
        // added here. Say so rather than reporting a mismatch with no detail.
        return format!(
            "sealed rows {}..={} count={} diverged from the physical cohort in a field this diagnostic does not yet decompose",
            seal.first_seq, seal.last_seq, seal.commitment_count
        );
    }
    parts.join(" ")
}

/// Explains a cohort that is *shorter* than its seal claims.
///
/// Issue #1876: this path reported only "claims N rows but only M remain",
/// which tells an operator how many rows vanished but not where. The sealed
/// ladder pins the position: the first index whose sealed digest no longer
/// matches is where the cohort diverged, and if the tail realigns after
/// skipping `d` positions then exactly `d` rows were dropped there, between two
/// sequences that can be named.
pub(crate) fn describe_truncated_cohort(
    seal: &RawCommitmentSeal,
    tail: &[RawCommitment],
) -> String {
    let mut parts = vec![format!(
        "sealed rows {}..={} count={} physical_rows_remaining={}",
        seal.first_seq,
        seal.last_seq,
        seal.commitment_count,
        tail.len()
    )];
    if tail.is_empty() {
        parts.push(
            "offending_sequences=unavailable reason=no physical commitment rows remain at this cursor"
                .to_owned(),
        );
        return parts.join(" ");
    }
    parts.push(format!(
        "physical_sequences=[{}]",
        join_seqs(&tail.iter().map(|row| row.seq).collect::<Vec<_>>())
    ));

    // Only a one-digest-per-row ladder pins an index to a sequence.
    let exact_ladder = seal.leaf_digests.len() == seal.commitment_count as usize
        && seal.commitment_count as usize <= LOCALIZATION_MAX_DIGESTS;
    if !exact_ladder {
        parts.push(format!(
            "offending_sequences=unavailable reason={}",
            if seal.leaf_digests.is_empty() {
                "this cohort was sealed by a pre-#1876 build carrying only the Merkle root, which \
                 cannot pin a position; the count divergence above is the finding"
            } else {
                "this cohort is larger than the localization ladder bound, so a sealed digest does \
                 not pin a single row"
            }
        ));
        return parts.join(" ");
    }

    let leaves: Vec<[u8; HASH_BYTES]> = tail.iter().map(commitment_leaf_hash).collect();
    let Some(first_divergent) = (0..leaves.len()).find(|i| leaves[*i] != seal.leaf_digests[*i])
    else {
        parts.push(
            "offending_sequences=none reason=every remaining row still matches its sealed digest in \
             order, so the missing rows are the tail of the cohort"
                .to_owned(),
        );
        parts.push(format!(
            "missing_tail_after_seq={} missing_row_count={}",
            tail[tail.len() - 1].seq,
            seal.commitment_count as usize - tail.len()
        ));
        return parts.join(" ");
    };

    let dropped = seal.commitment_count as usize - tail.len();
    let realigns =
        (first_divergent..leaves.len()).all(|i| leaves[i] == seal.leaf_digests[i + dropped]);
    if realigns {
        let before = if first_divergent == 0 {
            "<cohort start>".to_owned()
        } else {
            tail[first_divergent - 1].seq.to_string()
        };
        parts.push(format!(
            "dropped_row_count={dropped} dropped_at_sealed_index={first_divergent} \
             between_physical_seq={before} and_physical_seq={} \
             missing_sealed_leaf_digests=[{}] localization=exact",
            tail[first_divergent].seq,
            (first_divergent..first_divergent + dropped)
                .map(|i| hex32(&seal.leaf_digests[i]))
                .collect::<Vec<_>>()
                .join(",")
        ));
    } else {
        parts.push(format!(
            "first_divergent_sealed_index={first_divergent} \
             first_divergent_physical_seq={} \
             sealed_digest_at_that_index={} physical_digest={} \
             localization=position (the remaining rows do not realign after the gap, so more than a \
             simple deletion occurred)",
            tail[first_divergent].seq,
            hex32(&seal.leaf_digests[first_divergent]),
            hex32(&leaves[first_divergent])
        ));
    }
    parts.join(" ")
}

/// Names the exact rows whose content diverged, when the seal carries enough to
/// say so.
fn localize_content_divergence(
    seal: &RawCommitmentSeal,
    cohort: &[RawCommitment],
    leaves: &[[u8; HASH_BYTES]],
) -> String {
    if seal.leaf_digests.is_empty() {
        return format!(
            "offending_sequences=unavailable reason=this cohort was sealed by a pre-#1876 build \
             whose Ledger seal carries only the Merkle root, and a root alone cannot identify which \
             leaf moved (RFC 6962 localizes a leaf only via an inclusion proof, which this seal does \
             not store); cohorts sealed from this build carry a bounded per-row digest ladder and \
             name the exact sequence. physical_leaf_digests=[{}]",
            listed_leaf_digests(cohort, leaves)
        );
    }

    let buckets = seal.leaf_digests.len();
    if buckets != cohort.len().min(LOCALIZATION_MAX_DIGESTS) {
        return format!(
            "offending_sequences=unavailable reason=the sealed ladder has {buckets} digests but the \
             physical cohort of {} rows maps to {}; the cohort's row count itself diverged, so the \
             count mismatch above is the finding. physical_leaf_digests=[{}]",
            cohort.len(),
            cohort.len().min(LOCALIZATION_MAX_DIGESTS),
            listed_leaf_digests(cohort, leaves)
        );
    }

    let mut offenders: Vec<String> = Vec::new();
    let mut exact = true;
    for bucket in 0..buckets {
        let (start, end) = localization_bucket_bounds(cohort.len(), buckets, bucket);
        if start >= end {
            continue;
        }
        if merkle_root(&leaves[start..end]) == seal.leaf_digests[bucket] {
            continue;
        }
        if end - start == 1 {
            offenders.push(cohort[start].seq.to_string());
        } else {
            exact = false;
            offenders.push(format!(
                "{}..={}({} rows)",
                cohort[start].seq,
                cohort[end - 1].seq,
                end - start
            ));
        }
    }

    if offenders.is_empty() {
        return
            "offending_sequences=none reason=every sealed bucket digest matches the physical rows, so \
             the root diverged without any covered row changing; suspect the sealed root itself"
                .to_owned();
    }
    let precision = if exact {
        "exact"
    } else {
        "bucketed (cohort exceeds the localization ladder bound; each range narrows to the rows shown)"
    };
    format!(
        "offending_sequences=[{}] localization={precision}",
        offenders.join(",")
    )
}

fn listed_leaf_digests(cohort: &[RawCommitment], leaves: &[[u8; HASH_BYTES]]) -> String {
    let shown = cohort.len().min(MISMATCH_ROW_LISTING_CAP);
    let mut listed: Vec<String> = (0..shown)
        .map(|index| format!("{}:{}", cohort[index].seq, hex32(&leaves[index])))
        .collect();
    if cohort.len() > shown {
        listed.push(format!("…{} more", cohort.len() - shown));
    }
    listed.join(",")
}

fn join_seqs(seqs: &[u64]) -> String {
    let shown = seqs.len().min(MISMATCH_ROW_LISTING_CAP);
    let mut listed: Vec<String> = seqs[..shown].iter().map(u64::to_string).collect();
    if seqs.len() > shown {
        listed.push(format!("…{} more", seqs.len() - shown));
    }
    listed.join(",")
}

fn hex32(bytes: &[u8; HASH_BYTES]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
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
