use super::page;
use super::shared_reader;
use super::{
    MAX_RANGE_SCAN_BYTES, SstEntry, SstKeyState, SstLookupMetadata, SstPageReader, SstPointReader,
    clone_scan_bytes, materialized_entry_bytes, scan_reserve_failed,
};
use calyx_core::{CalyxError, Result};
use std::path::PathBuf;

use crate::storage_names::{SstName, classify_sst};

const SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL: usize = 10_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SstLevel {
    pub(super) files: Vec<LevelFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LevelFile {
    pub(super) path: PathBuf,
    lookup: Option<SstLookupMetadata>,
    lookup_retained: bool,
}

/// A level entry whose lookup index has already been read from disk, so that
/// installing it into a level costs no I/O. See [`SstLevel::prepare_with_lookup`].
#[derive(Debug)]
pub struct PreparedLevelFile(LevelFile);

#[derive(Debug)]
struct RankedRangeEntry {
    source_index: usize,
    entry: SstEntry,
}

#[derive(Debug)]
struct RankedKeyState {
    source_index: usize,
    state: SstKeyState,
}

impl LevelFile {
    fn without_lookup(path: PathBuf) -> Self {
        Self {
            path,
            lookup: None,
            lookup_retained: false,
        }
    }

    fn with_lookup(path: PathBuf) -> Result<Self> {
        let lookup = shared_reader(&path)?.lookup_metadata();
        Ok(Self {
            path,
            lookup,
            lookup_retained: true,
        })
    }

    fn may_contain(&self, key: &[u8]) -> bool {
        let Some(lookup) = &self.lookup else {
            return !self.lookup_retained;
        };
        key >= lookup.first_key.as_slice()
            && key <= lookup.last_key.as_slice()
            && lookup.bloom.may_contain(key)
    }

    pub(super) fn may_intersect(&self, start: &[u8], end: Option<&[u8]>) -> bool {
        if end.is_some_and(|end| start >= end) {
            return false;
        }
        let Some(lookup) = &self.lookup else {
            return !self.lookup_retained;
        };
        lookup.last_key.as_slice() >= start
            && end.is_none_or(|end| lookup.first_key.as_slice() < end)
    }

    pub(super) fn open_page_reader(&self) -> Result<Option<SstPageReader<'_>>> {
        match (&self.lookup, self.lookup_retained) {
            (Some(lookup), _) => SstPageReader::open(&self.path, lookup).map(Some),
            (None, true) => Ok(None),
            (None, false) => Err(calyx_core::CalyxError {
                code: "CALYX_ASTER_SST_PAGE_INDEX_MISSING",
                message: format!(
                    "candidate-bounded SST paging requires a retained validated lookup index for {}",
                    self.path.display()
                ),
                remediation: "reopen the vault with the required paged-CF lookup policy; do not fall back to a whole-file scan",
            }),
        }
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if !self.may_contain(key) {
            return Ok(None);
        }
        let Some(lookup) = &self.lookup else {
            return shared_reader(&self.path)?.get(key);
        };
        let Some(offset) = lookup.record_offset(key) else {
            return Ok(None);
        };
        SstPointReader::open(&self.path)?
            .read_value(offset, key)
            .map(Some)
    }
}

impl SstLevel {
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    pub fn from_oldest_first(files: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut files = files
            .into_iter()
            .map(LevelFile::without_lookup)
            .collect::<Vec<_>>();
        files.reverse();
        Self { files }
    }

    pub fn from_oldest_first_with_lookup(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let paths = paths.into_iter().collect::<Vec<_>>();
        let file_count = paths.len();
        let started_at = std::time::Instant::now();
        if file_count >= SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL {
            tracing::info!(
                code = "CALYX_ASTER_SST_LOOKUP_BUILD_START",
                file_count,
                "building eager SST lookup metadata"
            );
        }
        let mut files = Vec::new();
        let mut retained_lookup_files = 0_usize;
        let mut retained_empty_lookup_files = 0_usize;
        let mut retained_index_entries = 0_usize;
        let mut retained_lookup_estimated_heap_bytes = 0_usize;
        for (index, path) in paths.into_iter().enumerate() {
            let file = LevelFile::with_lookup(path)?;
            if file.lookup_retained {
                retained_lookup_files = retained_lookup_files.saturating_add(1);
            }
            if let Some(lookup) = file.lookup.as_ref() {
                retained_index_entries = retained_index_entries.saturating_add(lookup.len());
                retained_lookup_estimated_heap_bytes = retained_lookup_estimated_heap_bytes
                    .saturating_add(lookup.estimated_heap_bytes());
            } else {
                retained_empty_lookup_files = retained_empty_lookup_files.saturating_add(1);
            }
            files.push(file);
            let files_opened = index + 1;
            if file_count >= SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL
                && files_opened % SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL == 0
            {
                tracing::debug!(
                    code = "CALYX_ASTER_SST_LOOKUP_BUILD_PROGRESS",
                    file_count,
                    files_opened,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "eager SST lookup metadata build progress"
                );
            }
        }
        files.reverse();
        tracing::debug!(
            code = "CALYX_ASTER_SST_LOOKUP_BUILD_DONE",
            file_count,
            retained_lookup_files,
            retained_empty_lookup_files,
            retained_index_entries,
            retained_lookup_estimated_heap_bytes,
            elapsed_ms = started_at.elapsed().as_millis(),
            "completed eager SST lookup metadata build"
        );
        Ok(Self { files })
    }

    pub fn push(&mut self, path: PathBuf) {
        self.files.insert(0, LevelFile::without_lookup(path));
    }

    pub fn push_with_lookup(&mut self, path: PathBuf) -> Result<()> {
        self.files.insert(0, LevelFile::with_lookup(path)?);
        Ok(())
    }

    /// Reads a written SST's lookup index so the entry can later be inserted
    /// with no I/O.
    ///
    /// `push_with_lookup` opens and parses the SST inline, which is fine for a
    /// caller holding no lock but was 108 ms per commit under the router write
    /// lock once #1949 moved the SST write itself out (twelve files, each
    /// re-read immediately after being written). Splitting the read from the
    /// insert lets the caller pay it unlocked.
    pub fn prepare_with_lookup(path: PathBuf) -> Result<PreparedLevelFile> {
        Ok(PreparedLevelFile(LevelFile::with_lookup(path)?))
    }

    /// Inserts an entry prepared by [`Self::prepare_with_lookup`]. Pointer move
    /// only — no filesystem access.
    pub fn push_prepared(&mut self, prepared: PreparedLevelFile) {
        self.files.insert(0, prepared.0);
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        for file in &self.files {
            if let Some(value) = file.get(key)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    /// Re-derives the highest checkpointed commit that can affect a
    /// persistent search index and proves every router-flushed key has a
    /// commit-domain durable home. Relevant levels are opened with retained,
    /// fully validated lookup indexes before this method is called.
    pub(crate) fn prove_commit_domain_watermark(&self, durable_seq: u64) -> Result<u64> {
        let mut durable_files = Vec::new();
        let mut router_files = Vec::new();
        let mut watermark = 0_u64;
        for file in &self.files {
            match classify_sst(&file.path)? {
                Some(SstName::DurableBatch { seq, .. } | SstName::Compacted { seq })
                    if seq <= durable_seq =>
                {
                    watermark = watermark.max(seq);
                    durable_files.push(file);
                }
                Some(SstName::RouterLegacy { .. } | SstName::Flush { .. }) => {
                    router_files.push(file);
                }
                Some(SstName::DurableBatch { .. } | SstName::Compacted { .. }) | None => {}
            }
        }

        let mut missing_count = 0_u64;
        let mut samples = Vec::new();
        for router in router_files {
            let lookup = router.lookup.as_ref().ok_or_else(|| {
                calyx_core::CalyxError::aster_corrupt_shard(format!(
                    "persistent-search watermark migration lacks a validated lookup index for router SST {}",
                    router.path.display()
                ))
            })?;
            for key in lookup.keys() {
                if durable_files.iter().any(|file| {
                    file.lookup
                        .as_ref()
                        .is_some_and(|index| index.record_offset(key).is_some())
                }) {
                    continue;
                }
                missing_count += 1;
                if samples.len() < 3 {
                    samples.push(hex_prefix(key));
                }
            }
        }
        if missing_count != 0 {
            return Err(calyx_core::CalyxError {
                code: "CALYX_ASTER_DERIVED_CONTENT_MIGRATION_UNPROVEN",
                message: format!(
                    "persistent-search watermark migration found {missing_count} router-flushed key(s) with no commit-domain durable home, e.g. [{}]",
                    samples.join(", ")
                ),
                remediation: "preserve the vault and run verified CF compaction/recovery so every Base and quantized Slot key has a commit-domain SST before reopening; do not edit MANIFEST or SST files by hand",
            });
        }
        Ok(watermark)
    }

    pub(crate) fn values_for_key(&self, key: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut values = Vec::new();
        for file in &self.files {
            if !file.may_contain(key) {
                continue;
            }
            let reader = shared_reader(&file.path)?;
            if let Some(value) = reader.get(key)? {
                values.push(value);
            }
        }
        Ok(values)
    }

    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<Vec<SstEntry>> {
        self.collect_range(start, Some(end))
    }

    fn collect_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<SstEntry>> {
        // A per-SST ceiling is insufficient here: parallel materialization can
        // retain N individually legal vectors before the newest-wins merge, and
        // BTreeMap growth has no fallible reservation API. Stream every source
        // through borrowed validated rows into one fallibly grown aggregate,
        // charging duplicates too because they physically occupy memory until
        // the deterministic newest-wins sort/dedup completes.
        let mut retained_bytes = 0_usize;
        let mut ranked = Vec::<RankedRangeEntry>::new();
        for (source_index, file) in self.files.iter().enumerate() {
            if !file.may_intersect(start, end) {
                continue;
            }
            shared_reader(&file.path)?.visit_range_until(start, end, |key, value| {
                let next_record = materialized_entry_bytes::<RankedRangeEntry>(key, value);
                retained_bytes = retained_bytes.saturating_add(next_record);
                if retained_bytes > MAX_RANGE_SCAN_BYTES {
                    return Err(level_scan_budget_exceeded(retained_bytes, next_record));
                }
                ranked.try_reserve(1).map_err(scan_reserve_failed)?;
                ranked.push(RankedRangeEntry {
                    source_index,
                    entry: SstEntry {
                        key: clone_scan_bytes(key)?,
                        value: clone_scan_bytes(value)?,
                    },
                });
                Ok(())
            })?;
        }

        // Unstable sort is allocation-free. Source indices are part of the
        // total order, so equal keys deterministically retain the newest SST
        // (the level stores newest first) without relying on sort stability.
        ranked.sort_unstable_by(|left, right| {
            left.entry
                .key
                .cmp(&right.entry.key)
                .then_with(|| left.source_index.cmp(&right.source_index))
        });
        ranked.dedup_by(|later, earlier| later.entry.key == earlier.entry.key);

        let mut rows = Vec::new();
        rows.try_reserve_exact(ranked.len())
            .map_err(scan_reserve_failed)?;
        for row in ranked {
            rows.push(row.entry);
        }
        Ok(rows)
    }

    pub(crate) fn predecessor(
        &self,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Result<Option<SstEntry>> {
        let mut newest_at_greatest_key = None::<(usize, SstEntry)>;
        for (file_index, file) in self.files.iter().enumerate() {
            let Some(entry) = shared_reader(&file.path)?.predecessor(start, upper, inclusive)?
            else {
                continue;
            };
            let replace = newest_at_greatest_key
                .as_ref()
                .is_none_or(|(best_index, best)| {
                    entry.key > best.key || (entry.key == best.key && file_index < *best_index)
                });
            if replace {
                newest_at_greatest_key = Some((file_index, entry));
            }
        }
        Ok(newest_at_greatest_key.map(|(_, entry)| entry))
    }

    pub fn range_keys(&self, start: &[u8], end: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.range_keys_until(start, Some(end))
    }

    pub fn range_keys_until(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        let mut retained_bytes = 0_usize;
        let mut ranked = Vec::<RankedKeyState>::new();
        for (source_index, file) in self.files.iter().enumerate() {
            if !file.may_intersect(start, end) {
                continue;
            }
            shared_reader(&file.path)?.visit_range_until(start, end, |key, value| {
                let next_record = materialized_entry_bytes::<RankedKeyState>(key, &[]);
                retained_bytes = retained_bytes.saturating_add(next_record);
                if retained_bytes > MAX_RANGE_SCAN_BYTES {
                    return Err(level_scan_budget_exceeded(retained_bytes, next_record));
                }
                ranked.try_reserve(1).map_err(scan_reserve_failed)?;
                ranked.push(RankedKeyState {
                    source_index,
                    state: SstKeyState {
                        key: clone_scan_bytes(key)?,
                        is_tombstone: crate::mvcc::is_tombstone_value(value),
                    },
                });
                Ok(())
            })?;
        }

        ranked.sort_unstable_by(|left, right| {
            left.state
                .key
                .cmp(&right.state.key)
                .then_with(|| left.source_index.cmp(&right.source_index))
        });
        ranked.dedup_by(|later, earlier| later.state.key == earlier.state.key);

        let live_count = ranked.iter().filter(|row| !row.state.is_tombstone).count();
        let mut rows = Vec::new();
        rows.try_reserve_exact(live_count)
            .map_err(scan_reserve_failed)?;
        for row in ranked {
            if !row.state.is_tombstone {
                rows.push(row.state.key);
            }
        }
        Ok(rows)
    }

    pub fn range_page_until(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<SstEntry>> {
        self.range_page_with_overlay(start, end, after_key, limit, Vec::new())
    }

    pub(crate) fn range_candidate_page_until(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<SstEntry>> {
        page::range_candidate_page(self, start, end, after_key, limit)
    }

    pub(crate) fn range_page_with_overlay(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
        overlay: Vec<SstEntry>,
    ) -> Result<Vec<SstEntry>> {
        page::range_page(self, start, end, after_key, limit, overlay)
    }

    pub(crate) fn range_pages_with_overlay<F, E>(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
        overlay: Vec<SstEntry>,
        on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<SstEntry>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        page::range_pages(self, start, end, after_key, limit, overlay, on_page)
    }

    pub fn iter(&self) -> Result<Vec<SstEntry>> {
        self.collect_range(&[], None)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn file_paths_newest_first(&self) -> Vec<PathBuf> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }
}

fn level_scan_budget_exceeded(retained: usize, next_record: usize) -> CalyxError {
    CalyxError {
        code: "CALYX_ASTER_SCAN_MEMORY_BUDGET",
        message: format!(
            "SST level range merge exceeded the {MAX_RANGE_SCAN_BYTES}-byte aggregate materialization budget (retained={retained} bytes, next_record={next_record} bytes); refusing to allocate further"
        ),
        remediation: "use the bounded range-page API or narrow the key range, then compact excessive SST fan-in before retrying; the level merge fails closed instead of risking an allocator abort",
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let mut value = String::new();
    for byte in bytes.iter().take(12) {
        value.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > 12 {
        value.push_str("...");
    }
    value
}
