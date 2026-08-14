//! Blob layer: chunked payload + manifest.
//!
//! Large payloads are split into fixed-size chunks, each its own CF row, and a
//! single manifest row records the chunk count, total byte count, BLAKE3
//! content hash, and a cold-tier flag. All rows live in the `cf/blob` column
//! family under the `0x05` key-space discriminant.
//!
//! **Durability ordering.** Chunks are committed first in WAL-size-bounded
//! groups; the manifest is committed *last* in its own flush. The manifest is
//! the commit point — a crash between any chunk group and the manifest leaves
//! orphan chunks with no live manifest, so [`BlobLayer::blob_get`] sees no blob
//! rather than partial data.
//! Orphan chunks are reclaimed by the PH58 janitor. This is the content-
//! addressed "write blobs, reference by manifest last" pattern used by Ollama,
//! Docker, restic, and the AT Protocol.
//!
//! **Verification on read.** `blob_get` re-hashes the reassembled payload and
//! fails closed (`CALYX_ASTER_CORRUPT_SHARD`) on any mismatch, so silent
//! corruption is impossible.

mod codec;
mod stream;

use calyx_core::{CalyxError, Clock, Modality, Result, Seq};

pub use codec::{blob_row_range, chunk_key, collection_id, manifest_key};
use codec::{
    blob_too_large, chunk_prefix, corrupt, decode_manifest, encode_manifest, hash_payload,
    invalid_argument, ledger_payload, ledger_subject, require_blob_mode,
};
pub use stream::BlobChunkStream;

use crate::cf::{ColumnFamily, KeyRange};
use crate::collection::{
    CALYX_INVALID_ARGUMENT, Collection, CollectionMode, collection_has_lens,
    ingest_collection_constellation,
};
use crate::mvcc::tombstone_value;
use crate::vault::AsterVault;
use calyx_ledger::{ActorId, EntryKind, PayloadBuilder, RedactionPolicy, SubjectId};

/// Returned when a payload exceeds the hard per-blob ceiling.
pub const CALYX_BLOB_TOO_LARGE: &str = "CALYX_BLOB_TOO_LARGE";

const DISC_BLOB: u8 = 0x05;
const KIND_CHUNK: u8 = 0x00;
const KIND_MANIFEST: u8 = 0x01;
const BLOB_ID_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
/// Chunk-value budget for one durable group. Using half the WAL record ceiling
/// leaves more than 32 MiB for keys, row framing, encryption envelopes, and the
/// time-index row added by the commit layer. This is deliberately a byte bound,
/// not just a row count, so future chunk-size changes remain safe.
const BLOB_CHUNK_GROUP_VALUE_BYTES: usize = crate::wal::MAX_RECORD_BYTES / 2;

/// Fixed chunk size (256 KiB). Immutable once a vault has written its first
/// blob, so reads can address chunks by index without per-blob metadata.
pub const BLOB_CHUNK_SIZE: usize = 262_144;
/// Hard ceiling on a single blob (1 GiB) — fail closed above this.
pub const MAX_BLOB_BYTES: usize = 1 << 30;
/// Legacy `total_bytes (8) | chunk_count (4) | content_hash (32) | cold_tier (1)`.
const MANIFEST_VALUE_BYTES_V1: usize = 8 + 4 + HASH_BYTES + 1;
/// Current manifest appends `created_at_ms (8)` for retention decisions.
const MANIFEST_VALUE_BYTES: usize = MANIFEST_VALUE_BYTES_V1 + 8;

/// 16-byte content-or-caller-assigned blob identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlobId([u8; BLOB_ID_BYTES]);

impl BlobId {
    pub const fn from_bytes(bytes: [u8; BLOB_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != BLOB_ID_BYTES {
            return Err(invalid_argument(format!(
                "blob id must be {BLOB_ID_BYTES} bytes"
            )));
        }
        let mut out = [0_u8; BLOB_ID_BYTES];
        out.copy_from_slice(bytes);
        Ok(Self(out))
    }

    pub fn from_text(value: &str) -> Self {
        let hash = blake3::hash(value.as_bytes());
        let mut out = [0_u8; BLOB_ID_BYTES];
        out.copy_from_slice(&hash.as_bytes()[..BLOB_ID_BYTES]);
        Self(out)
    }

    pub const fn as_bytes(&self) -> &[u8; BLOB_ID_BYTES] {
        &self.0
    }
}

/// Decoded manifest row — the per-blob source of truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobManifest {
    pub total_bytes: u64,
    pub chunk_count: u32,
    pub content_hash: [u8; HASH_BYTES],
    pub cold_tier: bool,
    /// Unix milliseconds from the vault clock. `None` marks a legacy manifest.
    pub created_at_ms: Option<u64>,
}

/// Trusted result of a content-addressed blob write.
///
/// The identifier and manifest hash are derived by Aster from the same single
/// digest that was committed. Callers must consume this result instead of
/// hashing the payload or rereading the manifest independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobPutResult {
    pub seq: Seq,
    pub blob_id: BlobId,
    pub manifest: BlobManifest,
}

/// Coherent manifest and verified payload read from one pinned MVCC snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobReadResult {
    pub manifest: BlobManifest,
    pub data: Vec<u8>,
}

/// `(blob_id) -> chunked payload + manifest` layer over a `Blob` collection.
pub struct BlobLayer<'a, C: Clock> {
    vault: &'a AsterVault<C>,
}

impl<'a, C: Clock> BlobLayer<'a, C> {
    pub fn new(vault: &'a AsterVault<C>) -> Self {
        Self { vault }
    }

    /// Stores `data` under `blob_id`. Chunks are committed first, then the
    /// manifest in a separate commit, so a partial failure never leaves a live
    /// manifest pointing at missing chunks. Returns the manifest commit seq.
    pub fn blob_put(&self, col: &Collection, blob_id: BlobId, data: &[u8]) -> Result<Seq> {
        self.validate_put(col, data)?;
        let content_hash = hash_payload(data);
        self.blob_put_prepared(col, blob_id, data, content_hash)
            .map(|result| result.seq)
    }

    /// Stores a content-addressed blob and returns the exact committed outcome.
    ///
    /// Aster owns the only full-payload hash pass, derives `BlobId` from the
    /// first 16 digest bytes, uses the full digest in the manifest, and returns
    /// all three values without a post-commit point read.
    pub fn blob_put_content_addressed(
        &self,
        col: &Collection,
        data: &[u8],
    ) -> Result<BlobPutResult> {
        self.validate_put(col, data)?;
        let content_hash = hash_payload(data);
        let mut id = [0_u8; BLOB_ID_BYTES];
        id.copy_from_slice(&content_hash[..BLOB_ID_BYTES]);
        self.blob_put_prepared(col, BlobId::from_bytes(id), data, content_hash)
    }

    fn validate_put(&self, col: &Collection, data: &[u8]) -> Result<()> {
        if !collection_has_lens(col) {
            require_blob_mode(col)?;
        }
        if data.len() > MAX_BLOB_BYTES {
            return Err(blob_too_large(data.len()));
        }
        Ok(())
    }

    fn blob_put_prepared(
        &self,
        col: &Collection,
        blob_id: BlobId,
        data: &[u8],
        content_hash: [u8; HASH_BYTES],
    ) -> Result<BlobPutResult> {
        let chunk_count = if data.is_empty() {
            0
        } else {
            data.len().div_ceil(BLOB_CHUNK_SIZE)
        };
        let chunk_count = u32::try_from(chunk_count)
            .map_err(|_| invalid_argument("blob chunk count overflowed u32"))?;
        let manifest = BlobManifest {
            total_bytes: data.len() as u64,
            chunk_count,
            content_hash,
            cold_tier: false,
            created_at_ms: Some(self.vault.clock_now()),
        };

        if collection_has_lens(col) {
            let len = (data.len() as u64).to_be_bytes();
            let parts = [
                ("blob_id", blob_id.as_bytes().as_slice()),
                ("total_bytes", len.as_slice()),
                ("content_hash", content_hash.as_slice()),
                ("payload", data),
            ];
            let seq =
                ingest_collection_constellation(self.vault, col, "blob", &parts, Modality::Mixed)?;
            return Ok(BlobPutResult {
                seq,
                blob_id,
                manifest,
            });
        }
        let chunks: Vec<&[u8]> = if data.is_empty() {
            Vec::new()
        } else {
            data.chunks(BLOB_CHUNK_SIZE).collect()
        };
        debug_assert_eq!(chunks.len(), manifest.chunk_count as usize);

        // Phase 1: chunk rows in WAL-size-bounded groups, each durable before
        // the manifest exists. At 256 KiB chunks the value budget yields 128
        // rows/group, leaving half of the WAL record for framing/headroom.
        if !chunks.is_empty() {
            let chunks_per_group = BLOB_CHUNK_GROUP_VALUE_BYTES
                .checked_div(BLOB_CHUNK_SIZE)
                .filter(|count| *count > 0)
                .ok_or_else(|| invalid_argument("blob chunk size exceeds WAL group budget"))?;
            for (group_index, group) in chunks.chunks(chunks_per_group).enumerate() {
                let first_chunk = group_index * chunks_per_group;
                let value_bytes = group.iter().map(|bytes| bytes.len()).sum::<usize>();
                let chunk_rows = group.iter().enumerate().map(|(offset, bytes)| {
                    (
                        ColumnFamily::Blob,
                        chunk_key(col, blob_id, (first_chunk + offset) as u32),
                        bytes.to_vec(),
                    )
                });
                self.vault.write_cf_batch(chunk_rows).map_err(|error| CalyxError {
                    code: error.code,
                    message: format!(
                        "blob chunk group {group_index} failed: first_chunk={first_chunk} rows={} value_bytes={value_bytes}: {}",
                        group.len(), error.message
                    ),
                    remediation: error.remediation,
                })?;
            }
        }

        // Phase 2: the manifest is the commit point, with the Ledger entry.
        let key = manifest_key(col, blob_id);
        let value = encode_manifest(&manifest);
        let subject = ledger_subject(&key);
        let payload = ledger_payload(col, blob_id, &manifest);
        let seq = self.vault.write_cf_batch_with_ledger_entry(
            [(ColumnFamily::Blob, key, value)],
            EntryKind::Ingest,
            subject,
            payload,
            ActorId::Service("calyx-aster-blob".to_string()),
        )?;
        Ok(BlobPutResult {
            seq,
            blob_id,
            manifest,
        })
    }

    /// Reads the manifest only, without reassembling the payload.
    pub fn blob_manifest(&self, col: &Collection, blob_id: BlobId) -> Result<Option<BlobManifest>> {
        require_blob_mode(col)?;
        let snapshot = self.vault.snapshot_handle(self.vault.latest_seq())?;
        let Some(bytes) = self.vault.read_cf_snapshot(
            snapshot.snapshot(),
            ColumnFamily::Blob,
            &manifest_key(col, blob_id),
        )?
        else {
            return Ok(None);
        };
        decode_manifest(&bytes).map(Some)
    }

    /// Reassembles and returns the full payload, or `None` if there is no live
    /// manifest. Fails closed if a chunk is missing or the content hash does
    /// not match.
    pub fn blob_get(&self, col: &Collection, blob_id: BlobId) -> Result<Option<Vec<u8>>> {
        Ok(self.blob_read(col, blob_id)?.map(|result| result.data))
    }

    /// Reads the manifest and all chunks from one pinned snapshot, verifies
    /// length and content hash, and returns both without rereading the manifest.
    pub fn blob_read(&self, col: &Collection, blob_id: BlobId) -> Result<Option<BlobReadResult>> {
        require_blob_mode(col)?;
        let snapshot = self.vault.snapshot_handle(self.vault.latest_seq())?;
        let Some(manifest_bytes) = self.vault.read_cf_snapshot(
            snapshot.snapshot(),
            ColumnFamily::Blob,
            &manifest_key(col, blob_id),
        )?
        else {
            return Ok(None);
        };
        let manifest = decode_manifest(&manifest_bytes)?;
        let capacity = usize::try_from(manifest.total_bytes)
            .map_err(|_| corrupt("blob manifest total_bytes does not fit this platform"))?;
        let mut data = Vec::with_capacity(capacity);
        for idx in 0..manifest.chunk_count {
            let chunk = self
                .vault
                .read_cf_snapshot(
                    snapshot.snapshot(),
                    ColumnFamily::Blob,
                    &chunk_key(col, blob_id, idx),
                )?
                .ok_or_else(|| {
                    corrupt(format!(
                        "blob manifest claims {} chunks but chunk {idx} is missing",
                        manifest.chunk_count
                    ))
                })?;
            data.extend_from_slice(&chunk);
        }
        if data.len() as u64 != manifest.total_bytes {
            return Err(corrupt(format!(
                "blob reassembled to {} bytes but manifest says {}",
                data.len(),
                manifest.total_bytes
            )));
        }
        if hash_payload(&data) != manifest.content_hash {
            return Err(corrupt(
                "blob content hash mismatch on read — payload is corrupt",
            ));
        }
        Ok(Some(BlobReadResult { manifest, data }))
    }

    /// Tombstones every chunk row and the manifest in one batch. A subsequent
    /// `blob_get` reads back as absent. No-op (returns latest seq) if the blob
    /// does not exist.
    pub fn blob_delete(&self, col: &Collection, blob_id: BlobId) -> Result<Seq> {
        let Some(manifest) = self.blob_manifest(col, blob_id)? else {
            return Ok(self.vault.latest_seq());
        };
        let mut rows = Vec::with_capacity(manifest.chunk_count as usize + 1);
        for idx in 0..manifest.chunk_count {
            rows.push((
                ColumnFamily::Blob,
                chunk_key(col, blob_id, idx),
                tombstone_value(),
            ));
        }
        rows.push((
            ColumnFamily::Blob,
            manifest_key(col, blob_id),
            tombstone_value(),
        ));
        let key = manifest_key(col, blob_id);
        let subject = ledger_subject(&key);
        let payload = ledger_payload(col, blob_id, &manifest);
        self.vault.write_cf_batch_with_ledger_entry(
            rows,
            EntryKind::Ingest,
            subject,
            payload,
            ActorId::Service("calyx-aster-blob".to_string()),
        )
    }

    /// Lazy chunk iterator for streaming large blobs without a full in-memory
    /// load. Returns an empty stream if the blob is absent. Manifest-read
    /// errors surface here (we wrap in `Result` rather than swallow them).
    pub fn blob_stream_chunks(
        &self,
        col: &Collection,
        blob_id: BlobId,
    ) -> Result<BlobChunkStream<'_, C>> {
        let chunk_count = self
            .blob_manifest(col, blob_id)?
            .map_or(0, |manifest| manifest.chunk_count);
        Ok(BlobChunkStream {
            vault: self.vault,
            chunk_prefix: chunk_prefix(col, blob_id),
            chunk_count,
            next_idx: 0,
        })
    }
}
