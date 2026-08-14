use super::*;

pub(in crate::persisted::multi) fn search_segments(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query_tokens: &[Vec<f32>],
    k: usize,
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<IndexSearchHit>> {
    let manifest = read_segments_manifest(vault_dir, entry, manifest_base_seq, slot)?;
    let token_dim = entry.require_token_dim(slot)?;
    let mut seen = BTreeSet::new();
    let mut scored = Vec::new();
    let mut row_count = 0usize;
    let mut token_count = 0usize;
    for segment in &manifest.segments {
        bounds::ensure_segment_ref_bounded(slot, token_dim, segment)?;
        let path = checked_segment_path(vault_dir, &segment.index_rel, slot)?;
        let readback = binary::search_segment(binary::SegmentSearchRequest {
            path: &path,
            index_rel: &segment.index_rel,
            expected_sha256: &segment.sha256,
            slot,
            token_dim,
            base_seq: segment.base_seq,
            expected_rows: segment.row_count,
            expected_tokens: segment.token_count,
            query: query_tokens,
            k,
            candidates,
        })?;
        if !segment.ids.is_empty() && readback.ids != segment.ids.iter().copied().collect() {
            return Err(stale(format!(
                "persistent segmented multi sidecar {} IDs do not match its manifest; rebuild the vault search indexes",
                segment.index_rel
            )));
        }
        for cx_id in readback.ids {
            if !seen.insert(cx_id) {
                return Err(stale(format!(
                    "persistent segmented multi sidecars repeat {cx_id}; rebuild the vault search indexes"
                )));
            }
        }
        row_count = row_count
            .checked_add(readback.row_count)
            .ok_or_else(|| stale("persistent segmented multi row_count overflow"))?;
        token_count = token_count
            .checked_add(readback.token_count)
            .ok_or_else(|| stale("persistent segmented multi token_count overflow"))?;
        scored.extend(readback.scored);
        scored = top_k(scored, k);
    }
    if row_count != manifest.row_count {
        return Err(stale(format!(
            "persistent segmented multi manifest row_count {} != scanned row count {}; rebuild the vault search indexes",
            manifest.row_count, row_count
        )));
    }
    if token_count != manifest.token_count {
        return Err(stale(format!(
            "persistent segmented multi manifest token_count {} != scanned token count {}; rebuild the vault search indexes",
            manifest.token_count, token_count
        )));
    }
    Ok(ranked(scored))
}
