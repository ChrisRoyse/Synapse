use super::page;
use super::{SstEntry, SstKeyState, SstLookupMetadata, SstPointReader, SstReader};
use calyx_core::Result;
use rayon::prelude::*;
use std::collections::BTreeMap;
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
}

impl LevelFile {
    fn without_lookup(path: PathBuf) -> Self {
        Self { path, lookup: None }
    }

    fn with_lookup(path: PathBuf) -> Result<Self> {
        let lookup = SstReader::open(&path)?.lookup_metadata();
        Ok(Self { path, lookup })
    }

    fn may_contain(&self, key: &[u8]) -> bool {
        let Some(lookup) = &self.lookup else {
            return true;
        };
        key >= lookup.first_key.as_slice()
            && key <= lookup.last_key.as_slice()
            && lookup.bloom.may_contain(key)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if !self.may_contain(key) {
            return Ok(None);
        }
        let Some(lookup) = &self.lookup else {
            return SstReader::open(&self.path)?.get(key);
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
        for (index, path) in paths.into_iter().enumerate() {
            files.push(LevelFile::with_lookup(path)?);
            let files_opened = index + 1;
            if file_count >= SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL
                && files_opened % SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL == 0
            {
                tracing::info!(
                    code = "CALYX_ASTER_SST_LOOKUP_BUILD_PROGRESS",
                    file_count,
                    files_opened,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "eager SST lookup metadata build progress"
                );
            }
        }
        files.reverse();
        if file_count >= SST_LOOKUP_BUILD_PROGRESS_FILE_INTERVAL {
            tracing::info!(
                code = "CALYX_ASTER_SST_LOOKUP_BUILD_DONE",
                file_count,
                elapsed_ms = started_at.elapsed().as_millis(),
                "completed eager SST lookup metadata build"
            );
        }
        Ok(Self { files })
    }

    pub fn push(&mut self, path: PathBuf) {
        self.files.insert(0, LevelFile::without_lookup(path));
    }

    pub fn push_with_lookup(&mut self, path: PathBuf) -> Result<()> {
        self.files.insert(0, LevelFile::with_lookup(path)?);
        Ok(())
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
            let reader = SstReader::open(&file.path)?;
            if let Some(value) = reader.get(key)? {
                values.push(value);
            }
        }
        Ok(values)
    }

    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<Vec<SstEntry>> {
        let mut per_file = self
            .files
            .par_iter()
            .enumerate()
            .map(|(index, file)| -> Result<(usize, Vec<SstEntry>)> {
                Ok((index, SstReader::open(&file.path)?.range(start, end)?))
            })
            .collect::<Result<Vec<_>>>()?;
        per_file.sort_by_key(|(index, _)| *index);

        let mut rows = BTreeMap::new();
        for (_, entries) in per_file {
            for entry in entries {
                rows.entry(entry.key).or_insert(entry.value);
            }
        }
        Ok(rows
            .into_iter()
            .map(|(key, value)| SstEntry { key, value })
            .collect())
    }

    pub(crate) fn predecessor(
        &self,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Result<Option<SstEntry>> {
        let mut newest_at_greatest_key = None::<(usize, SstEntry)>;
        for (file_index, file) in self.files.iter().enumerate() {
            let Some(entry) = SstReader::open(&file.path)?.predecessor(start, upper, inclusive)?
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
        let mut per_file = self
            .files
            .par_iter()
            .enumerate()
            .map(|(index, file)| -> Result<(usize, Vec<SstKeyState>)> {
                Ok((
                    index,
                    SstReader::open(&file.path)?.range_key_states_until(start, end)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        per_file.sort_by_key(|(index, _)| *index);

        let mut rows = BTreeMap::<Vec<u8>, bool>::new();
        for (_, entries) in per_file {
            for entry in entries {
                rows.entry(entry.key).or_insert(entry.is_tombstone);
            }
        }
        Ok(rows
            .into_iter()
            .filter_map(|(key, is_tombstone)| (!is_tombstone).then_some(key))
            .collect())
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
        let mut rows = BTreeMap::new();
        for file in &self.files {
            for entry in SstReader::open(&file.path)?.iter()? {
                rows.entry(entry.key).or_insert(entry.value);
            }
        }
        Ok(rows
            .into_iter()
            .map(|(key, value)| SstEntry { key, value })
            .collect())
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn file_paths_newest_first(&self) -> Vec<PathBuf> {
        self.files.iter().map(|file| file.path.clone()).collect()
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
