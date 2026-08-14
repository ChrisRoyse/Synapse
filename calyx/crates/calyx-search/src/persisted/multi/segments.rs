use super::*;

#[path = "segments/path.rs"]
mod path;
use path::{checked_rel, checked_segment_path};
#[path = "segments/manifest.rs"]
mod manifest;
use manifest::validate_segments_manifest_shape;
#[path = "segments/bounds.rs"]
mod bounds;
#[path = "segments/search.rs"]
mod search;
#[path = "segments/writer.rs"]
mod writer;
pub(super) use bounds::ensure_entry_bounded;
pub(in crate::persisted) use bounds::ensure_streaming_row_bounded;
pub(super) use search::search_segments;
pub(in crate::persisted) use writer::{SegmentFlush, StreamingSegmentsWriter};

const MULTI_SEGMENTS_FORMAT: &str = "calyx-search-multi-maxsim-segments-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct MultiSegmentsManifest {
    format: String,
    slot: u16,
    token_dim: u32,
    base_seq: u64,
    row_count: usize,
    token_count: usize,
    segments: Vec<MultiSegmentRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct MultiSegmentRef {
    pub(super) index_rel: String,
    pub(super) sha256: String,
    pub(super) base_seq: u64,
    pub(super) row_count: usize,
    pub(super) token_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) ids: Vec<CxId>,
}

struct SegmentManifestBuild {
    token_dim: u32,
    row_count: usize,
    token_count: usize,
    base_seq: u64,
    segments: Vec<MultiSegmentRef>,
}

pub(super) struct EncodedMultiRow {
    pub(super) cx_id: CxId,
    pub(super) token_count: u32,
    pub(super) bytes: Vec<u8>,
}

pub(super) fn referenced_segment_artifacts(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    slot: SlotId,
) -> CliResult<Vec<PathBuf>> {
    let manifest = read_segments_manifest(vault_dir, entry, entry.built_at_seq, slot)?;
    manifest
        .segments
        .iter()
        .map(|segment| checked_segment_path(vault_dir, &segment.index_rel, slot))
        .collect()
}

fn write_encoded_binary_segment(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    token_dim: u32,
    rows: &[EncodedMultiRow],
    base_seq: u64,
    ordinal: usize,
) -> CliResult<MultiSegmentRef> {
    let path = root.join(format!(
        "slot_{:05}_seq_{base_seq:020}_seg_{ordinal:05}_n_{:010}.multi.bin",
        slot.get(),
        rows.len()
    ));
    let token_count = rows.iter().try_fold(0usize, |total, row| {
        total
            .checked_add(row.token_count as usize)
            .ok_or_else(|| stale("streaming multi segment token_count overflow"))
    })?;
    let sha256 =
        binary::write_encoded_binary_atomic_hashed(&path, slot, token_dim, rows, base_seq)?;
    Ok(MultiSegmentRef {
        index_rel: rel(vault_dir, &path)?,
        sha256,
        base_seq,
        row_count: rows.len(),
        token_count,
        ids: rows.iter().map(|row| row.cx_id).collect(),
    })
}

fn write_segments_manifest(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    build: SegmentManifestBuild,
) -> CliResult<SearchIndexEntry> {
    let manifest = MultiSegmentsManifest {
        format: MULTI_SEGMENTS_FORMAT.to_string(),
        slot: slot.get(),
        token_dim: build.token_dim,
        base_seq: build.base_seq,
        row_count: build.row_count,
        token_count: build.token_count,
        segments: build.segments,
    };
    validate_segments_manifest_shape(
        &manifest,
        slot,
        build.token_dim,
        build.base_seq,
        build.row_count,
        build.token_count,
    )?;
    let path = root.join(format!(
        "slot_{:05}_seq_{:020}_n_{:010}.multi.segments.json",
        slot.get(),
        build.base_seq,
        build.row_count
    ));
    let sha256 = write_json_atomic_hashed(&path, &manifest)?;
    Ok(SearchIndexEntry::multi_segments(
        slot,
        build.token_dim,
        build.row_count,
        build.token_count,
        build.base_seq,
        rel(vault_dir, &path)?,
        sha256,
    ))
}

pub(super) fn read_segments_manifest(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult<MultiSegmentsManifest> {
    entry.require_kind("multi_maxsim_segments", slot)?;
    let path = checked_segment_path(vault_dir, entry.require_index_rel(slot)?, slot)?;
    let bytes = fs::read(&path)?;
    let actual = sha256_hex(&bytes);
    let expected = entry.require_sha256(slot)?;
    if actual != expected {
        return Err(stale(format!(
            "persistent segmented multi manifest sha256 {actual} != manifest {expected}; rebuild the vault search indexes"
        )));
    }
    let manifest: MultiSegmentsManifest = serde_json::from_slice(&bytes).map_err(|err| {
        stale(format!(
            "persistent segmented multi manifest {} is not valid JSON: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    validate_segments_manifest_shape(
        &manifest,
        slot,
        entry.require_token_dim(slot)?,
        manifest_base_seq,
        entry.len,
        entry.token_count.unwrap_or_default(),
    )?;
    Ok(manifest)
}

pub(super) fn validate_segment_files(
    vault_dir: &Path,
    slot: SlotId,
    token_dim: u32,
    manifest: &MultiSegmentsManifest,
) -> CliResult {
    for segment in &manifest.segments {
        bounds::ensure_segment_ref_bounded(slot, token_dim, segment)?;
        let path = checked_segment_path(vault_dir, &segment.index_rel, slot)?;
        let expected =
            bounds::segment_estimated_bytes(token_dim, segment.row_count, segment.token_count)?;
        let actual = fs::metadata(&path)?.len();
        if actual != expected {
            return Err(stale(format!(
                "persistent segmented multi sidecar {} has {actual} bytes, expected {expected}; rebuild the vault search indexes",
                segment.index_rel
            )));
        }
        let summary = binary::summarize_binary_path(
            &path,
            &segment.sha256,
            slot,
            token_dim,
            Some(segment.row_count as u64),
            Some(segment.token_count as u64),
        )?;
        if summary.base_seq != segment.base_seq {
            return Err(stale(format!(
                "persistent segmented multi sidecar {} has base_seq {}, expected {}; rebuild the vault search indexes",
                segment.index_rel, summary.base_seq, segment.base_seq
            )));
        }
        let expected_ids = segment.ids.iter().copied().collect::<BTreeSet<_>>();
        if summary.ids != expected_ids {
            return Err(stale(format!(
                "persistent segmented multi sidecar {} IDs do not match its manifest; rebuild the vault search indexes",
                segment.index_rel
            )));
        }
    }
    Ok(())
}
