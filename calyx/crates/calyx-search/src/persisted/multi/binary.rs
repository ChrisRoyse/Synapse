use super::*;

pub(in crate::persisted::multi) const MULTI_BINARY_MAGIC: &[u8; 16] = b"CYX_MULTI_BIN_V1";

#[derive(Clone, Copy, Debug)]
pub(super) struct BinaryHeader {
    slot: u16,
    token_dim: u32,
    base_seq: u64,
    row_count: u64,
    token_count: u64,
}

pub(super) fn is_binary_sidecar(index_rel: &str) -> bool {
    index_rel.ends_with(".multi.bin")
}

pub(super) fn write_encoded_binary_atomic_hashed(
    path: &Path,
    slot: SlotId,
    token_dim: u32,
    rows: &[super::segments::EncodedMultiRow],
    base_seq: u64,
) -> CliResult<String> {
    let token_count = rows.iter().try_fold(0usize, |total, row| {
        total
            .checked_add(row.token_count as usize)
            .ok_or_else(|| stale("streaming multi sidecar token_count overflow"))
    })?;
    write_atomic_hashed(path, |writer| {
        writer.write_all(MULTI_BINARY_MAGIC)?;
        write_u16(writer, slot.get())?;
        write_u32(writer, token_dim)?;
        write_u64(writer, base_seq)?;
        write_u64(writer, rows.len() as u64)?;
        write_u64(writer, token_count as u64)?;
        for row in rows {
            let encoded = EncodedMultiSlotVector::new(&row.bytes).map_err(|error| {
                CalyxError::aster_corrupt_shard(format!(
                    "slot {slot} cx {} has malformed encoded multi payload during sidecar write: {}",
                    row.cx_id, error.message
                ))
            })?;
            if encoded.token_dim() != token_dim || encoded.token_count() != row.token_count {
                return Err(stale(format!(
                    "slot {slot} cx {} encoded multi shape changed while buffered: token_dim={} expected={token_dim}, token_count={} expected={}",
                    row.cx_id,
                    encoded.token_dim(),
                    encoded.token_count(),
                    row.token_count
                )));
            }
            writer.write_all(row.cx_id.as_bytes())?;
            write_u32(writer, row.token_count)?;
            for value in encoded.components() {
                if !value.is_finite() {
                    return Err(CalyxError::lens_numerical_invariant(format!(
                        "slot {slot} cx {} has non-finite multi token component",
                        row.cx_id
                    ))
                    .into());
                }
                writer.write_all(&value.to_le_bytes())?;
            }
        }
        Ok(())
    })
}

pub(super) fn search_binary(
    vault_dir: &Path,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
    query: &[Vec<f32>],
    k: usize,
    candidates: Option<&BTreeSet<CxId>>,
) -> CliResult<Vec<IndexSearchHit>> {
    let path = sidecar_path(vault_dir, entry, slot)?;
    let file = File::open(&path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let header = read_binary_header_hashed(&path, &mut reader, &mut hasher)?;
    validate_binary_header(&header, entry, manifest_base_seq, slot)?;

    let mut seen = BTreeSet::new();
    let mut observed_tokens = 0u64;
    let mut scored = Vec::new();
    for _ in 0..header.row_count {
        let cx_id = read_cx_id(&path, &mut reader, &mut hasher)?;
        if !seen.insert(cx_id) {
            return Err(stale(format!(
                "persistent binary multi sidecar repeats {cx_id}; rebuild the vault search indexes"
            )));
        }
        let row_token_count = read_u32(&path, &mut reader, &mut hasher)? as u64;
        observed_tokens = observed_tokens
            .checked_add(row_token_count)
            .ok_or_else(|| stale("persistent binary multi sidecar token_count overflow"))?;
        if observed_tokens > header.token_count {
            return Err(stale(format!(
                "persistent binary multi sidecar token_count exceeds header {}; rebuild the vault search indexes",
                header.token_count
            )));
        }
        let tokens = read_tokens(
            &path,
            &mut reader,
            &mut hasher,
            slot,
            cx_id,
            header.token_dim,
            row_token_count,
        )?;
        if candidates.is_none_or(|allowed| allowed.contains(&cx_id)) {
            scored.push((cx_id, MaxSimIndex::maxsim(query, &tokens)));
        }
    }
    if observed_tokens != header.token_count {
        return Err(stale(format!(
            "persistent binary multi sidecar token_count {observed_tokens} != header {}; rebuild the vault search indexes",
            header.token_count
        )));
    }
    ensure_no_trailing_bytes(&path, &mut reader, &mut hasher)?;
    let actual = finish_sha256_hex(hasher);
    let expected = entry.require_sha256(slot)?;
    if actual != expected {
        return Err(stale(format!(
            "persistent binary multi sidecar sha256 {actual} != manifest {expected}; rebuild the vault search indexes"
        )));
    }
    Ok(ranked(top_k(scored, k)))
}

/// Independently verify and score one bounded segmented sidecar.
///
/// The segment is consumed row-by-row from disk. Only the current row's token
/// payload is decoded, and the score accumulator is pruned to the requested
/// result window, so query memory is bounded by the writer's single-row limit
/// plus `O(k)` rather than the size of the generation. The complete file hash
/// and every structural count are still verified before scores are accepted.
pub(in crate::persisted::multi) struct SegmentSearchRequest<'a> {
    pub path: &'a Path,
    pub index_rel: &'a str,
    pub expected_sha256: &'a str,
    pub slot: SlotId,
    pub token_dim: u32,
    pub base_seq: u64,
    pub expected_rows: usize,
    pub expected_tokens: usize,
    pub query: &'a [Vec<f32>],
    pub k: usize,
    pub candidates: Option<&'a BTreeSet<CxId>>,
}

pub(in crate::persisted::multi) fn search_segment(
    request: SegmentSearchRequest<'_>,
) -> CliResult<SegmentSearchReadback> {
    let SegmentSearchRequest {
        path,
        index_rel,
        expected_sha256,
        slot,
        token_dim,
        base_seq,
        expected_rows,
        expected_tokens,
        query,
        k,
        candidates,
    } = request;
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let header = read_binary_header_hashed(path, &mut reader, &mut hasher)?;
    if header.slot != slot.get()
        || header.token_dim != token_dim
        || header.base_seq != base_seq
        || header.row_count != expected_rows as u64
        || header.token_count != expected_tokens as u64
    {
        return Err(stale(format!(
            "persistent segmented multi sidecar {index_rel} header slot={} token_dim={} base_seq={} rows={} tokens={} does not match manifest slot={} token_dim={token_dim} base_seq={base_seq} rows={expected_rows} tokens={expected_tokens}; rebuild the vault search indexes",
            header.slot,
            header.token_dim,
            header.base_seq,
            header.row_count,
            header.token_count,
            slot.get()
        )));
    }
    let mut ids = BTreeSet::new();
    let mut observed_tokens = 0usize;
    let mut scored = Vec::new();
    for _ in 0..expected_rows {
        let cx_id = read_cx_id(path, &mut reader, &mut hasher)?;
        if !ids.insert(cx_id) {
            return Err(stale(format!(
                "persistent segmented multi sidecar {index_rel} repeats {cx_id}; rebuild the vault search indexes"
            )));
        }
        let row_token_count = read_u32(path, &mut reader, &mut hasher)? as usize;
        observed_tokens = observed_tokens
            .checked_add(row_token_count)
            .ok_or_else(|| stale("persistent segmented multi token_count overflow"))?;
        if observed_tokens > expected_tokens {
            return Err(stale(format!(
                "persistent segmented multi sidecar {index_rel} token_count exceeds manifest {expected_tokens}; rebuild the vault search indexes"
            )));
        }
        let tokens = read_tokens(
            path,
            &mut reader,
            &mut hasher,
            slot,
            cx_id,
            token_dim,
            row_token_count as u64,
        )?;
        if k != 0 && candidates.is_none_or(|allowed| allowed.contains(&cx_id)) {
            scored.push((cx_id, MaxSimIndex::maxsim(query, &tokens)));
            prune_score_window(&mut scored, k);
        }
    }
    if observed_tokens != expected_tokens {
        return Err(stale(format!(
            "persistent segmented multi sidecar {index_rel} token_count {observed_tokens} != manifest {expected_tokens}; rebuild the vault search indexes"
        )));
    }
    ensure_no_trailing_bytes(path, &mut reader, &mut hasher)?;
    let actual = finish_sha256_hex(hasher);
    if actual != expected_sha256 {
        return Err(stale(format!(
            "persistent segmented multi sidecar sha256 {actual} != manifest {expected_sha256} for {index_rel}; rebuild the vault search indexes"
        )));
    }
    Ok(SegmentSearchReadback {
        ids,
        row_count: expected_rows,
        token_count: observed_tokens,
        scored: top_k(scored, k),
    })
}

fn prune_score_window(scored: &mut Vec<(CxId, f32)>, k: usize) {
    let prune_at = k.saturating_mul(2).max(k.saturating_add(1));
    if scored.len() >= prune_at {
        *scored = top_k(std::mem::take(scored), k);
    }
}

pub(in crate::persisted::multi) struct SegmentSearchReadback {
    pub(in crate::persisted::multi) ids: BTreeSet<CxId>,
    pub(in crate::persisted::multi) row_count: usize,
    pub(in crate::persisted::multi) token_count: usize,
    pub(in crate::persisted::multi) scored: Vec<(CxId, f32)>,
}

#[path = "binary/segments.rs"]
mod segments;

pub(super) use segments::summarize_binary_path;

pub(super) fn read_binary_header_unhashed(path: &Path) -> CliResult<BinaryHeader> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 16];
    reader.read_exact(&mut magic).map_err(|err| {
        stale(format!(
            "persistent binary multi sidecar {} is unreadable: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    if &magic != MULTI_BINARY_MAGIC {
        return Err(stale(format!(
            "persistent binary multi sidecar {} has invalid magic; rebuild the vault search indexes",
            path.display()
        )));
    }
    let slot = read_u16_unhashed(path, &mut reader)?;
    let token_dim = read_u32_unhashed(path, &mut reader)?;
    let base_seq = read_u64_unhashed(path, &mut reader)?;
    let row_count = read_u64_unhashed(path, &mut reader)?;
    let token_count = read_u64_unhashed(path, &mut reader)?;
    Ok(BinaryHeader {
        slot,
        token_dim,
        base_seq,
        row_count,
        token_count,
    })
}

fn read_binary_header_hashed<R: Read>(
    path: &Path,
    reader: &mut R,
    hasher: &mut Sha256,
) -> CliResult<BinaryHeader> {
    let mut magic = [0u8; 16];
    read_exact_hashed(path, reader, hasher, &mut magic)?;
    if &magic != MULTI_BINARY_MAGIC {
        return Err(stale(format!(
            "persistent binary multi sidecar {} has invalid magic; rebuild the vault search indexes",
            path.display()
        )));
    }
    let slot = read_u16(path, reader, hasher)?;
    let token_dim = read_u32(path, reader, hasher)?;
    let base_seq = read_u64(path, reader, hasher)?;
    let row_count = read_u64(path, reader, hasher)?;
    let token_count = read_u64(path, reader, hasher)?;
    Ok(BinaryHeader {
        slot,
        token_dim,
        base_seq,
        row_count,
        token_count,
    })
}

pub(super) fn validate_binary_header(
    header: &BinaryHeader,
    entry: &SearchIndexEntry,
    manifest_base_seq: u64,
    slot: SlotId,
) -> CliResult {
    if header.slot != slot.get() || entry.slot != slot.get() {
        return Err(stale(format!(
            "persistent binary multi sidecar slot {} / entry slot {} != query slot {}",
            header.slot,
            entry.slot,
            slot.get()
        )));
    }
    let entry_token_dim = entry.require_token_dim(slot)?;
    if header.token_dim != entry_token_dim {
        return Err(stale(format!(
            "persistent binary multi sidecar token_dim {} != manifest token_dim {entry_token_dim}; rebuild the vault search indexes",
            header.token_dim
        )));
    }
    if header.base_seq != manifest_base_seq || entry.built_at_seq != manifest_base_seq {
        return Err(stale(format!(
            "persistent binary multi sidecar seq {} / entry seq {} != manifest seq {manifest_base_seq}; rebuild the vault search indexes",
            header.base_seq, entry.built_at_seq
        )));
    }
    if header.row_count != entry.len as u64 {
        return Err(stale(format!(
            "persistent binary multi sidecar row len {} != manifest len {}; rebuild the vault search indexes",
            header.row_count, entry.len
        )));
    }
    if entry
        .token_count
        .is_some_and(|count| header.token_count != count as u64)
    {
        return Err(stale(format!(
            "persistent binary multi sidecar token_count {} != manifest token_count {}; rebuild the vault search indexes",
            header.token_count,
            entry.token_count.unwrap_or_default()
        )));
    }
    Ok(())
}

fn read_tokens<R: Read>(
    path: &Path,
    reader: &mut R,
    hasher: &mut Sha256,
    slot: SlotId,
    cx_id: CxId,
    token_dim: u32,
    row_token_count: u64,
) -> CliResult<Vec<Vec<f32>>> {
    let row_token_count_usize = usize::try_from(row_token_count).map_err(|_| {
        stale(format!(
            "persistent binary multi sidecar row {cx_id} token count does not fit usize; rebuild the vault search indexes"
        ))
    })?;
    let token_dim_usize = token_dim as usize;
    let mut tokens = Vec::with_capacity(row_token_count_usize);
    for _ in 0..row_token_count_usize {
        let mut token = Vec::with_capacity(token_dim_usize);
        for _ in 0..token_dim_usize {
            let value = read_f32(path, reader, hasher)?;
            if !value.is_finite() {
                return Err(CalyxError::lens_numerical_invariant(format!(
                    "persistent binary multi row {cx_id} slot {slot} has non-finite component"
                ))
                .into());
            }
            token.push(value);
        }
        tokens.push(token);
    }
    Ok(tokens)
}

fn read_cx_id<R: Read>(path: &Path, reader: &mut R, hasher: &mut Sha256) -> CliResult<CxId> {
    let mut bytes = [0u8; 16];
    read_exact_hashed(path, reader, hasher, &mut bytes)?;
    Ok(CxId::from_bytes(bytes))
}

fn read_u16_unhashed<R: Read>(path: &Path, reader: &mut R) -> CliResult<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes).map_err(|err| {
        stale(format!(
            "persistent binary multi sidecar {} is truncated: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32_unhashed<R: Read>(path: &Path, reader: &mut R) -> CliResult<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes).map_err(|err| {
        stale(format!(
            "persistent binary multi sidecar {} is truncated: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_unhashed<R: Read>(path: &Path, reader: &mut R) -> CliResult<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes).map_err(|err| {
        stale(format!(
            "persistent binary multi sidecar {} is truncated: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u16<R: Read>(path: &Path, reader: &mut R, hasher: &mut Sha256) -> CliResult<u16> {
    let mut bytes = [0u8; 2];
    read_exact_hashed(path, reader, hasher, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(path: &Path, reader: &mut R, hasher: &mut Sha256) -> CliResult<u32> {
    let mut bytes = [0u8; 4];
    read_exact_hashed(path, reader, hasher, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(path: &Path, reader: &mut R, hasher: &mut Sha256) -> CliResult<u64> {
    let mut bytes = [0u8; 8];
    read_exact_hashed(path, reader, hasher, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_f32<R: Read>(path: &Path, reader: &mut R, hasher: &mut Sha256) -> CliResult<f32> {
    let mut bytes = [0u8; 4];
    read_exact_hashed(path, reader, hasher, &mut bytes)?;
    Ok(f32::from_le_bytes(bytes))
}

fn write_u16<W: Write>(writer: &mut W, value: u16) -> CliResult {
    Ok(writer.write_all(&value.to_le_bytes())?)
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> CliResult {
    Ok(writer.write_all(&value.to_le_bytes())?)
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> CliResult {
    Ok(writer.write_all(&value.to_le_bytes())?)
}

fn read_exact_hashed<R: Read>(
    path: &Path,
    reader: &mut R,
    hasher: &mut Sha256,
    buf: &mut [u8],
) -> CliResult {
    reader.read_exact(buf).map_err(|err| {
        stale(format!(
            "persistent binary multi sidecar {} is truncated: {err}; rebuild the vault search indexes",
            path.display()
        ))
    })?;
    hasher.update(buf);
    Ok(())
}

fn ensure_no_trailing_bytes<R: Read>(
    path: &Path,
    reader: &mut R,
    hasher: &mut Sha256,
) -> CliResult {
    let mut byte = [0u8; 1];
    match reader.read(&mut byte) {
        Ok(0) => Ok(()),
        Ok(read) => {
            hasher.update(&byte[..read]);
            Err(stale(format!(
                "persistent binary multi sidecar {} has trailing bytes; rebuild the vault search indexes",
                path.display()
            )))
        }
        Err(err) => Err(stale(format!(
            "persistent binary multi sidecar {} could not be checked for trailing bytes: {err}; rebuild the vault search indexes",
            path.display()
        ))),
    }
}

fn finish_sha256_hex(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}
