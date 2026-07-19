//! Persisted Base CF page index used by bounded readback commands.
//!
//! The index is not a cache fallback. Bounded readers either verify this
//! physical source of truth against the current Base-source freshness digest
//! (the ordered Base SST identity set plus the Base WAL-row tail) and the
//! referenced SST or WAL bytes, or they fail closed with a
//! `CALYX_BASE_PAGE_INDEX_*` error. Unrelated ledger advancement does not stale
//! the index; any real Base mutation does.

mod format;
mod readback;
mod sst_scan;
mod types;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use calyx_core::{CalyxError, Result};
use sha2::{Digest, Sha256};

use crate::cf::ColumnFamily;
use crate::ledger_head::read_head_anchor;
use crate::manifest::ManifestStore;
use crate::mvcc::is_tombstone_value;
use crate::sst::shared_reader;
use crate::storage_names::sst_order_key;
use crate::vault::encode::decode_write_batch_refs;
use crate::wal::stream_records_after;

use format::{
    corrupt, decode_hex, hex_bytes, missing, now_ms, relative_path, remove_path, sha256_hex, stale,
    sync_parent, write_bytes_file, write_json_file, write_json_file_atomic,
};
use readback::{read_page, visit_source_values};
use sst_scan::list_base_sst_files;
pub use types::{
    BASE_PAGE_INDEX_DIR, BASE_PAGE_INDEX_GENERATIONS_DIR, BASE_PAGE_INDEX_MANIFEST,
    BasePageIndexBuildProgress, BasePageIndexEntry, BasePageIndexManifest, BasePageIndexPage,
    BasePageIndexPageRef, BasePageIndexSource, DEFAULT_BASE_PAGE_INDEX_PAGE_SIZE,
};
use types::{
    GENERATION_INDEX_VERSION, INDEX_MAGIC, INDEX_VERSION, LEGACY_INDEX_VERSION, PRIOR_INDEX_VERSION,
};

/// Domain separation for the Base-source freshness digest. Bump the trailing
/// version whenever the digest input layout changes.
const BASE_SOURCE_DIGEST_DOMAIN: &[u8] = b"calyx.base_source_digest.v1\0";

static GENERATION_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
struct IndexedValue {
    value_sha256_hex: String,
    tombstoned: bool,
    source: BasePageIndexSource,
}

struct BuildSnapshot {
    ledger_head_height: u64,
    ledger_head_tip_hash_hex: String,
    base_source_digest_hex: String,
    base_durable_seq: u64,
    base_sst_files: usize,
    wal_records: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationBoundary {
    GenerationManifestSynced,
    GenerationPublished,
    CommitPointPublished,
}

pub fn build_base_page_index(
    vault: &Path,
    page_size: usize,
    mut progress: impl FnMut(BasePageIndexBuildProgress) -> Result<()>,
) -> Result<BasePageIndexManifest> {
    if page_size == 0 {
        return Err(corrupt("Base page index page size must be at least 1"));
    }
    let _guard = crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
    let (ledger_head_height, ledger_head_tip_hash_hex) = current_head(vault)?;
    let sst_files = list_base_sst_files(vault)?;
    progress(BasePageIndexBuildProgress::ScanStarted {
        sst_files: sst_files.len(),
        ledger_head_height,
    })?;

    let mut rows = BTreeMap::<Vec<u8>, IndexedValue>::new();
    for (index, file) in sst_files.iter().enumerate() {
        let order = sst_order_key(file)?.ok_or_else(|| {
            corrupt(format!(
                "Base SST {} has no canonical order key",
                file.display()
            ))
        })?;
        let path = relative_path(vault, file);
        for (record_offset, entry) in shared_reader(file)?.iter_with_offsets()? {
            let tombstoned = is_tombstone_value(&entry.value);
            rows.insert(
                entry.key,
                IndexedValue {
                    value_sha256_hex: sha256_hex(&entry.value),
                    tombstoned,
                    source: BasePageIndexSource::Sst {
                        path: path.clone(),
                        order_epoch: order.epoch,
                        order_seq: order.seq,
                        order_class_rank: order.class_rank,
                        order_index: order.index,
                        record_offset: Some(record_offset),
                    },
                },
            );
        }
        let scanned = index + 1;
        if scanned == 1 || scanned == sst_files.len() || scanned % 1000 == 0 {
            progress(BasePageIndexBuildProgress::SstScanned {
                scanned_sst_files: scanned,
                total_sst_files: sst_files.len(),
                current_rows: rows.len(),
            })?;
        }
    }

    let durable_seq = ManifestStore::open(vault).load_current()?.durable_seq;
    let wal_records = stream_records_after(vault.join("wal"), durable_seq, |record| {
        for row in decode_write_batch_refs(&record.payload)? {
            if row.cf != ColumnFamily::Base {
                continue;
            }
            let tombstoned = is_tombstone_value(row.value);
            let encoded_offset = u64::try_from(row.encoded_offset).map_err(|_| {
                corrupt("encoded WAL write-row offset exceeds u64 during Base index build")
            })?;
            let row_offset = record
                .start_offset
                .checked_add(crate::wal::RECORD_HEADER_BYTES)
                .and_then(|offset| offset.checked_add(encoded_offset))
                .ok_or_else(|| corrupt("physical WAL Base row offset overflow"))?;
            rows.insert(
                row.key.to_vec(),
                IndexedValue {
                    value_sha256_hex: sha256_hex(row.value),
                    tombstoned,
                    source: BasePageIndexSource::Wal {
                        path: relative_path(vault, &record.segment_path),
                        seq: record.seq,
                        start_offset: record.start_offset,
                        end_offset: record.end_offset,
                        row_offset: Some(row_offset),
                    },
                },
            );
        }
        Ok(())
    })?;
    progress(BasePageIndexBuildProgress::WalScanned {
        wal_records,
        current_rows: rows.len(),
    })?;

    // Freeze the exact Base-source freshness key readers will recompute. The
    // durable-commit lock held by this function makes this digest describe the
    // same physical Base state that was folded into `rows` above.
    let (base_source_digest_hex, base_durable_seq, digest_sst_files) =
        compute_base_source_digest(vault)?;
    if digest_sst_files != sst_files.len() {
        return Err(corrupt(format!(
            "Base SST set changed during index build: folded {} files but freshness digest saw {}",
            sst_files.len(),
            digest_sst_files
        )));
    }

    let manifest = write_index(
        vault,
        page_size,
        rows,
        BuildSnapshot {
            ledger_head_height,
            ledger_head_tip_hash_hex,
            base_source_digest_hex,
            base_durable_seq,
            base_sst_files: sst_files.len(),
            wal_records,
        },
        progress,
    )?;
    Ok(manifest)
}

/// Computes a content-derived freshness key for the current Base column
/// family. Callers must hold the vault durable-commit lock so the SST listing
/// and WAL tail form one consistent physical snapshot.
fn compute_base_source_digest(vault: &Path) -> Result<(String, u64, usize)> {
    let sst_files = list_base_sst_files(vault)?;
    let mut hasher = Sha256::new();
    hasher.update(BASE_SOURCE_DIGEST_DOMAIN);
    hasher.update((sst_files.len() as u64).to_le_bytes());
    for file in &sst_files {
        let order = sst_order_key(file)?.ok_or_else(|| {
            corrupt(format!(
                "Base SST {} has no canonical order key",
                file.display()
            ))
        })?;
        let relative = relative_path(vault, file);
        let len = fs::metadata(file)
            .map_err(|error| {
                CalyxError::disk_pressure(format!(
                    "stat Base SST {} for freshness digest: {error}",
                    file.display()
                ))
            })?
            .len();
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update([order.epoch, order.class_rank]);
        hasher.update(order.seq.to_le_bytes());
        hasher.update((order.index as u64).to_le_bytes());
        hasher.update(len.to_le_bytes());
    }

    let durable_seq = ManifestStore::open(vault).load_current()?.durable_seq;
    let mut base_wal_rows = 0_u64;
    stream_records_after(vault.join("wal"), durable_seq, |record| {
        for row in decode_write_batch_refs(&record.payload)? {
            if row.cf != ColumnFamily::Base {
                continue;
            }
            base_wal_rows = base_wal_rows
                .checked_add(1)
                .ok_or_else(|| corrupt("Base WAL freshness row count overflow"))?;
            let tombstoned = is_tombstone_value(row.value);
            hasher.update((row.key.len() as u64).to_le_bytes());
            hasher.update(row.key);
            hasher.update(Sha256::digest(row.value));
            hasher.update([u8::from(tombstoned)]);
        }
        Ok(())
    })?;
    hasher.update(base_wal_rows.to_le_bytes());
    Ok((hex_bytes(&hasher.finalize()), durable_seq, sst_files.len()))
}

pub fn read_base_page_index_manifest(vault: &Path) -> Result<BasePageIndexManifest> {
    read_manifest_file(&manifest_path(vault))
}

pub fn read_indexed_base_rows(vault: &Path, limit: usize) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    if limit == 0 {
        return Ok(BTreeMap::new());
    }
    let _guard = crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
    let manifest = read_manifest_file(&manifest_path(vault))?;
    validate_base_source_unchanged(vault, &manifest)?;
    validate_current_read_format(&manifest)?;
    let mut rows = BTreeMap::new();
    for page_ref in &manifest.pages {
        for (key, value) in read_live_page_rows(vault, page_ref)? {
            if rows.insert(key, value).is_some() {
                return Err(corrupt("Base page index contains a duplicate live key"));
            }
            if rows.len() == limit {
                return Ok(rows);
            }
        }
    }
    Ok(rows)
}

pub fn read_indexed_base_rows_for_keys(
    vault: &Path,
    keys: &[Vec<u8>],
) -> Result<BTreeMap<Vec<u8>, Option<Vec<u8>>>> {
    let mut rows = BTreeMap::new();
    visit_indexed_base_rows_for_keys(vault, keys, |key, value| {
        rows.insert(key.to_vec(), value);
        Ok(())
    })?;
    Ok(rows)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectedBaseRowsVisit {
    pub unique_keys: usize,
    pub touched_pages: usize,
    pub source_files: usize,
    pub live_rows: usize,
    pub missing_rows: usize,
}

/// Visit selected Base rows without retaining their complete values.
///
/// Requested keys are sorted and deduplicated, each touched page is decoded
/// once, and backing files are opened once in physical-offset order. Visitor
/// order is therefore physical rather than key order. Every source value is
/// hash-validated before the visitor receives ownership, and is dropped before
/// the next row unless the visitor deliberately retains it.
pub fn visit_indexed_base_rows_for_keys<E>(
    vault: &Path,
    keys: &[Vec<u8>],
    mut visitor: impl FnMut(&[u8], Option<Vec<u8>>) -> std::result::Result<(), E>,
) -> std::result::Result<SelectedBaseRowsVisit, E>
where
    E: From<CalyxError>,
{
    let _guard = crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
    let manifest = read_manifest_file(&manifest_path(vault))?;
    validate_base_source_unchanged(vault, &manifest)?;
    validate_current_read_format(&manifest)?;
    let unique_keys = keys.iter().collect::<std::collections::BTreeSet<_>>();
    let mut stats = SelectedBaseRowsVisit {
        unique_keys: unique_keys.len(),
        ..SelectedBaseRowsVisit::default()
    };
    let mut cached_page = None::<(usize, BasePageIndexPage)>;
    let mut selected = Vec::new();
    for key in unique_keys {
        let key_hex = hex_bytes(key);
        let Some(page_index) = manifest.pages.iter().position(|page| {
            page.first_key_hex.as_str() <= key_hex.as_str()
                && key_hex.as_str() <= page.last_key_hex.as_str()
        }) else {
            stats.missing_rows += 1;
            visitor(key, None)?;
            continue;
        };
        if cached_page.as_ref().map(|(index, _)| *index) != Some(page_index) {
            cached_page = Some((page_index, read_page(vault, &manifest.pages[page_index])?));
            stats.touched_pages += 1;
        }
        let page = &cached_page.as_ref().expect("selected page is cached").1;
        let Some(entry) = page.entries.iter().find(|entry| entry.key_hex == key_hex) else {
            stats.missing_rows += 1;
            visitor(key, None)?;
            continue;
        };
        selected.push((key.clone(), entry.clone()));
    }
    let source_stats = visit_source_values(vault, selected, |key, value| {
        if is_tombstone_value(&value) {
            stats.missing_rows += 1;
            visitor(key, None)
        } else {
            stats.live_rows += 1;
            visitor(key, Some(value))
        }
    })?;
    stats.source_files = source_stats.source_files;
    Ok(stats)
}

pub fn visit_indexed_base_row_pages<E>(
    vault: &Path,
    mut visitor: impl FnMut(usize, Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<bool, E>,
) -> std::result::Result<usize, E>
where
    E: From<CalyxError>,
{
    let _guard = crate::file_lock::FileLockGuard::acquire(&durable_commit_lock_path(vault))?;
    let manifest = read_manifest_file(&manifest_path(vault))?;
    validate_base_source_unchanged(vault, &manifest)?;
    validate_current_read_format(&manifest)?;
    let mut live_rows = 0usize;
    for page_ref in &manifest.pages {
        let rows = read_live_page_rows(vault, page_ref)?;
        if rows.is_empty() {
            continue;
        }
        let row_count = rows.len();
        if !visitor(live_rows, rows)? {
            return Ok(live_rows + row_count);
        }
        live_rows += row_count;
    }
    Ok(live_rows)
}

fn write_index(
    vault: &Path,
    page_size: usize,
    rows: BTreeMap<Vec<u8>, IndexedValue>,
    snapshot: BuildSnapshot,
    progress: impl FnMut(BasePageIndexBuildProgress) -> Result<()>,
) -> Result<BasePageIndexManifest> {
    write_index_with_hook(vault, page_size, rows, snapshot, progress, |_| Ok(()))
}

fn write_index_with_hook(
    vault: &Path,
    page_size: usize,
    rows: BTreeMap<Vec<u8>, IndexedValue>,
    snapshot: BuildSnapshot,
    mut progress: impl FnMut(BasePageIndexBuildProgress) -> Result<()>,
    mut publication_hook: impl FnMut(PublicationBoundary) -> Result<()>,
) -> Result<BasePageIndexManifest> {
    let built_at_unix_ms = now_ms()?;
    let generation = format!(
        "generation-{:020}-{:032}-{:010}-{:020}",
        snapshot.ledger_head_height,
        built_at_unix_ms,
        std::process::id(),
        GENERATION_NONCE.fetch_add(1, Ordering::Relaxed)
    );
    let index_root = vault.join(BASE_PAGE_INDEX_DIR);
    let generations_root = index_root.join(BASE_PAGE_INDEX_GENERATIONS_DIR);
    fs::create_dir_all(&generations_root).map_err(|error| {
        CalyxError::disk_pressure(format!(
            "create Base page index generations directory {}: {error}",
            generations_root.display()
        ))
    })?;
    sync_parent(&index_root)?;
    sync_parent(&generations_root)?;
    let staging = generations_root.join(format!(".{generation}.tmp"));
    fs::create_dir(&staging).map_err(|error| {
        CalyxError::disk_pressure(format!(
            "create-new Base page index generation staging directory {}: {error}",
            staging.display()
        ))
    })?;
    let published_prefix = format!("{BASE_PAGE_INDEX_GENERATIONS_DIR}/{generation}");
    let mut pages = Vec::new();
    let mut chunk = Vec::with_capacity(page_size);
    let mut page_index = 0;
    let mut live_entries = 0;
    let total_entries = rows.len();
    for (key, indexed) in rows {
        let tombstoned = indexed.tombstoned;
        if !tombstoned {
            live_entries += 1;
        }
        chunk.push(BasePageIndexEntry {
            key_hex: hex_bytes(&key),
            value_sha256_hex: indexed.value_sha256_hex,
            tombstoned,
            source: indexed.source,
        });
        if chunk.len() == page_size {
            write_page(
                &staging,
                &published_prefix,
                page_index,
                std::mem::take(&mut chunk),
                &mut pages,
            )?;
            emit_page_progress(
                &mut progress,
                page_index,
                pages.last().expect("page written"),
            )?;
            page_index += 1;
        }
    }
    if !chunk.is_empty() {
        write_page(&staging, &published_prefix, page_index, chunk, &mut pages)?;
        emit_page_progress(
            &mut progress,
            page_index,
            pages.last().expect("page written"),
        )?;
    }
    let manifest = BasePageIndexManifest {
        magic: INDEX_MAGIC.to_string(),
        version: INDEX_VERSION,
        generation: Some(generation.clone()),
        base_source_digest_hex: snapshot.base_source_digest_hex,
        base_durable_seq: snapshot.base_durable_seq,
        ledger_head_height: snapshot.ledger_head_height,
        ledger_head_tip_hash_hex: snapshot.ledger_head_tip_hash_hex,
        page_size,
        total_entries,
        live_entries,
        tombstone_entries: total_entries.saturating_sub(live_entries),
        base_sst_files: snapshot.base_sst_files,
        wal_records: snapshot.wal_records,
        built_at_unix_ms,
        pages,
    };
    let generation_manifest = staging.join(BASE_PAGE_INDEX_MANIFEST);
    write_json_file(&generation_manifest, &manifest)?;
    sync_parent(&generation_manifest)?;
    publication_hook(PublicationBoundary::GenerationManifestSynced)?;
    let published_generation = generations_root.join(&generation);
    crate::fsync::publish_path(
        &staging,
        &published_generation,
        "immutable Base page index generation",
        crate::fsync::PublishMode::CreateNew,
    )?;
    sync_parent(&published_generation)?;
    publication_hook(PublicationBoundary::GenerationPublished)?;
    validate_immutable_generation(vault, &published_generation, &manifest)?;
    let commit_point = index_root.join(BASE_PAGE_INDEX_MANIFEST);
    write_json_file_atomic(&commit_point, &manifest)?;
    publication_hook(PublicationBoundary::CommitPointPublished)?;
    validate_published_generation(vault, &manifest)?;
    prune_obsolete_generations(&index_root, &generation)?;
    progress(BasePageIndexBuildProgress::Complete {
        total_entries: manifest.total_entries,
        live_entries: manifest.live_entries,
        pages: manifest.pages.len(),
    })?;
    Ok(manifest)
}

fn read_live_page_rows(
    vault: &Path,
    page_ref: &BasePageIndexPageRef,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let page = read_page(vault, page_ref)?;
    let entries = page
        .entries
        .into_iter()
        .map(|entry| {
            let key = decode_hex(&entry.key_hex, "Base page index key")?;
            Ok((key, entry))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut rows = Vec::with_capacity(page_ref.live_entry_count);
    visit_source_values(vault, entries, |key, value| {
        if !is_tombstone_value(&value) {
            rows.push((key.to_vec(), value));
        }
        Ok::<_, CalyxError>(())
    })?;
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    if rows.len() != page_ref.live_entry_count {
        return Err(corrupt(format!(
            "Base page index page {} expected {} live entries, got {}",
            page_ref.path,
            page_ref.live_entry_count,
            rows.len()
        )));
    }
    Ok(rows)
}

fn write_page(
    dir: &Path,
    published_prefix: &str,
    page_index: usize,
    entries: Vec<BasePageIndexEntry>,
    pages: &mut Vec<BasePageIndexPageRef>,
) -> Result<()> {
    let first_key_hex = entries
        .first()
        .map(|entry| entry.key_hex.clone())
        .unwrap_or_default();
    let last_key_hex = entries
        .last()
        .map(|entry| entry.key_hex.clone())
        .unwrap_or_default();
    let live_entry_count = entries.iter().filter(|entry| !entry.tombstoned).count();
    let page = BasePageIndexPage { entries };
    let bytes = serde_json::to_vec_pretty(&page)
        .map_err(|error| corrupt(format!("encode Base page index page: {error}")))?;
    let file_name = format!("page-{page_index:08}.json");
    write_bytes_file(&dir.join(&file_name), &bytes)?;
    pages.push(BasePageIndexPageRef {
        path: format!("{published_prefix}/{file_name}"),
        first_key_hex,
        last_key_hex,
        entry_count: page.entries.len(),
        live_entry_count,
        sha256_hex: sha256_hex(&bytes),
    });
    Ok(())
}

fn emit_page_progress(
    progress: &mut impl FnMut(BasePageIndexBuildProgress) -> Result<()>,
    page_index: usize,
    page: &BasePageIndexPageRef,
) -> Result<()> {
    progress(BasePageIndexBuildProgress::PageWritten {
        page_index,
        entry_count: page.entry_count,
        live_entry_count: page.live_entry_count,
    })
}

fn read_manifest_file(path: &Path) -> Result<BasePageIndexManifest> {
    if !path.exists() {
        return Err(missing(format!(
            "Base page index manifest is missing at {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        CalyxError::disk_pressure(format!("read Base page index manifest: {error}"))
    })?;
    let manifest: BasePageIndexManifest = serde_json::from_slice(&bytes)
        .map_err(|error| corrupt(format!("decode Base page index manifest: {error}")))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &BasePageIndexManifest) -> Result<()> {
    if manifest.magic != INDEX_MAGIC {
        return Err(corrupt(format!(
            "Base page index manifest magic {} is not {INDEX_MAGIC}",
            manifest.magic
        )));
    }
    match manifest.version {
        LEGACY_INDEX_VERSION => {
            if manifest.generation.is_some() {
                return Err(corrupt(
                    "legacy Base page index manifest unexpectedly names a generation",
                ));
            }
            for page in &manifest.pages {
                if page.path.contains('/') || page.path.contains('\\') {
                    return Err(corrupt(format!(
                        "legacy Base page index has non-canonical page path {}",
                        page.path
                    )));
                }
            }
        }
        GENERATION_INDEX_VERSION | PRIOR_INDEX_VERSION | INDEX_VERSION => {
            validate_generation_manifest(manifest)?;
        }
        other => {
            return Err(corrupt(format!(
                "Base page index version {other} is not supported (legacy={LEGACY_INDEX_VERSION}, generation={GENERATION_INDEX_VERSION}, prior={PRIOR_INDEX_VERSION}, current={INDEX_VERSION})",
            )));
        }
    }
    if manifest.version == INDEX_VERSION {
        let digest = &manifest.base_source_digest_hex;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(corrupt(format!(
                "Base page index manifest has invalid Base source digest {digest:?} (expected 64 hex chars)"
            )));
        }
    }
    if manifest.page_size == 0 {
        return Err(corrupt("Base page index manifest page_size is zero"));
    }
    if manifest.live_entries + manifest.tombstone_entries != manifest.total_entries {
        return Err(corrupt("Base page index manifest row counts do not add up"));
    }
    if manifest
        .pages
        .iter()
        .map(|page| page.entry_count)
        .sum::<usize>()
        != manifest.total_entries
    {
        return Err(corrupt("Base page index page counts do not add up"));
    }
    let mut previous_last = None;
    for page in &manifest.pages {
        if page.live_entry_count > page.entry_count {
            return Err(corrupt(format!(
                "Base page index page {} has {} live entries but only {} total entries",
                page.path, page.live_entry_count, page.entry_count
            )));
        }
        if page.entry_count == 0 || page.first_key_hex > page.last_key_hex {
            return Err(corrupt(format!(
                "Base page index page {} has an empty or reversed key range",
                page.path
            )));
        }
        if let Some(last) = previous_last
            && last >= page.first_key_hex.as_str()
        {
            return Err(corrupt(format!(
                "Base page index page {} overlaps or is out of order after key {last}",
                page.path
            )));
        }
        if page.sha256_hex.len() != 64
            || !page.sha256_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(corrupt(format!(
                "Base page index page {} has invalid sha256 {}",
                page.path, page.sha256_hex
            )));
        }
        previous_last = Some(page.last_key_hex.as_str());
    }
    Ok(())
}

fn validate_generation_manifest(manifest: &BasePageIndexManifest) -> Result<()> {
    let generation = manifest.generation.as_deref().ok_or_else(|| {
        corrupt("current Base page index manifest does not name an immutable generation")
    })?;
    if !generation.starts_with("generation-")
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(corrupt(format!(
            "Base page index generation {generation:?} is not a canonical generation identifier"
        )));
    }
    let prefix = format!("{BASE_PAGE_INDEX_GENERATIONS_DIR}/{generation}/");
    for page in &manifest.pages {
        let Some(file_name) = page.path.strip_prefix(&prefix) else {
            return Err(corrupt(format!(
                "Base page index generation {generation} references page outside itself: {}",
                page.path
            )));
        };
        if file_name.contains('/')
            || file_name.contains('\\')
            || !file_name.starts_with("page-")
            || !file_name.ends_with(".json")
        {
            return Err(corrupt(format!(
                "Base page index generation {generation} has non-canonical page path {}",
                page.path
            )));
        }
    }
    Ok(())
}

fn validate_published_generation(vault: &Path, expected: &BasePageIndexManifest) -> Result<()> {
    let published = read_manifest_file(&manifest_path(vault))?;
    if &published != expected {
        return Err(corrupt(
            "published Base page index commit point differs from the completed generation manifest",
        ));
    }
    for page_ref in &published.pages {
        let page = read_page(vault, page_ref)?;
        if page.entries.len() != page_ref.entry_count {
            return Err(corrupt(format!(
                "published Base page index page {} failed independent entry-count readback",
                page_ref.path
            )));
        }
    }
    Ok(())
}

fn validate_immutable_generation(
    vault: &Path,
    generation_dir: &Path,
    expected: &BasePageIndexManifest,
) -> Result<()> {
    let generation_manifest = read_manifest_file(&generation_dir.join(BASE_PAGE_INDEX_MANIFEST))?;
    if &generation_manifest != expected {
        return Err(corrupt(format!(
            "immutable Base page index generation {} does not match its completed in-memory manifest",
            generation_dir.display()
        )));
    }
    for page_ref in &generation_manifest.pages {
        read_page(vault, page_ref)?;
    }
    Ok(())
}

fn prune_obsolete_generations(index_root: &Path, current_generation: &str) -> Result<()> {
    let generations = index_root.join(BASE_PAGE_INDEX_GENERATIONS_DIR);
    for entry in fs::read_dir(&generations).map_err(|error| {
        CalyxError::disk_pressure(format!(
            "list Base page index generations {}: {error}",
            generations.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            CalyxError::disk_pressure(format!("read Base page index generation entry: {error}"))
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == current_generation {
            continue;
        }
        if !(name.starts_with("generation-")
            || (name.starts_with(".generation-") && name.ends_with(".tmp")))
        {
            return Err(corrupt(format!(
                "refusing to prune unexpected Base page index generation entry {}",
                entry.path().display()
            )));
        }
        remove_path(&entry.path())?;
    }
    for entry in fs::read_dir(index_root).map_err(|error| {
        CalyxError::disk_pressure(format!(
            "list Base page index root {}: {error}",
            index_root.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            CalyxError::disk_pressure(format!("read Base page index root entry: {error}"))
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("page-") && name.ends_with(".json") {
            remove_path(&entry.path())?;
        }
    }
    sync_parent(&generations.join(".prune-sync"))?;
    Ok(())
}

/// Fails closed when the current Base column-family bytes no longer match the
/// Base-source digest frozen into the manifest.
///
/// Freshness is bound to Base content, not the global ledger head: an
/// Answer/Guard/Assay-only append leaves the recomputed digest identical and
/// the index is accepted without a rebuild, while any real Base
/// put/tombstone/compaction or SST corruption changes the digest and this
/// returns `CALYX_BASE_PAGE_INDEX_STALE`.
fn validate_base_source_unchanged(vault: &Path, manifest: &BasePageIndexManifest) -> Result<()> {
    let (digest_hex, durable_seq, base_sst_files) = compute_base_source_digest(vault)?;
    if digest_hex != manifest.base_source_digest_hex {
        return Err(stale(format!(
            "Base page index was built over Base source digest {} (durable_seq {}, {} SST files) but current Base source digest is {} (durable_seq {}, {} SST files); the Base column family changed since the index was built",
            manifest.base_source_digest_hex,
            manifest.base_durable_seq,
            manifest.base_sst_files,
            digest_hex,
            durable_seq,
            base_sst_files
        )));
    }
    Ok(())
}

fn validate_current_read_format(manifest: &BasePageIndexManifest) -> Result<()> {
    if manifest.version != INDEX_VERSION {
        return Err(stale(format!(
            "Base page index version {} lacks the exact physical offsets required by current version {INDEX_VERSION}; rebuild the index before readback",
            manifest.version
        )));
    }
    Ok(())
}

fn manifest_path(vault: &Path) -> PathBuf {
    vault
        .join(BASE_PAGE_INDEX_DIR)
        .join(BASE_PAGE_INDEX_MANIFEST)
}

fn durable_commit_lock_path(vault: &Path) -> PathBuf {
    vault.join("locks").join("durable.commit.lock")
}

fn current_head(vault: &Path) -> Result<(u64, String)> {
    let Some(anchor) = read_head_anchor(vault)? else {
        return Ok((0, hex_bytes(&[0_u8; 32])));
    };
    Ok((anchor.height, hex_bytes(&anchor.tip_hash)))
}
