use super::{insert_ledger_bytes, parse_aster_ledger_seq};
use crate::cf::ledger_key;
use crate::sst::level::SstLevel;
use crate::sst::{SstLookupMetadata, SstReader};
use crate::storage_names::{SstName, classify_sst, sst_order_key};
use calyx_core::{CalyxError, Result as CalyxResult};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Per-tier resolution stats for one targeted ledger point read (#1112).
///
/// Every tier reports how many seqs it was asked to resolve, how many it
/// actually resolved, how many SST files it opened doing so, and how long it
/// took. FSV asserts the resolution path from this record: on a healthy vault
/// the `commit_ordered` tier resolves durable SST rows with O(k log n) file
/// opens, WAL-tail rows are checked before the exhaustive audit, and
/// `complete_scan` is absent on healthy startup hydration.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LedgerPointReadTierStats {
    pub tier: &'static str,
    /// Seqs still unresolved when this tier ran (0 = tier skipped).
    pub wanted: usize,
    /// Seqs this tier resolved.
    pub resolved: usize,
    /// SST files opened by this tier (footer/metadata probes and row reads).
    pub files_opened: usize,
    pub elapsed_ms: u64,
}

/// Ordered record of every tier a targeted ledger point read ran (#1112).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct LedgerPointReadTrace {
    pub tiers: Vec<LedgerPointReadTierStats>,
}

impl LedgerPointReadTrace {
    pub(super) fn record(
        &mut self,
        tier: &'static str,
        wanted: usize,
        resolved: usize,
        files_opened: usize,
        started: Instant,
    ) {
        self.tiers.push(LedgerPointReadTierStats {
            tier,
            wanted,
            resolved,
            files_opened,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
    }
}

pub(super) fn read_sst_ledger_rows(
    ledger_dirs: &[PathBuf],
    wanted: &BTreeSet<u64>,
    rows: &mut BTreeMap<u64, Vec<u8>>,
    trace: &mut LedgerPointReadTrace,
) -> CalyxResult<()> {
    // Tier 1 (`probable_name`): O(1) stat probes for SSTs whose file name IS
    // the wanted seq. Only resolves on vaults where ledger seqs and WAL commit
    // seqs coincide (e.g. single-put-per-commit test vaults); kept because it
    // costs two stats per seq and avoids the index build entirely there.
    {
        let started = Instant::now();
        let before = rows.len();
        let candidates = probable_ledger_sst_candidates(ledger_dirs, wanted)?;
        let files_opened = candidates.len();
        read_rows_from_candidate_level(candidates, wanted, rows)?;
        trace.record(
            "probable_name",
            wanted.len(),
            rows.len() - before,
            files_opened,
            started,
        );
    }

    // Tier 2 (`commit_ordered`): the #1112 fix. Ledger CF keys are 8-byte
    // big-endian ledger seqs appended in commit order, so the durable-batch
    // SSTs (sorted by their commit-seq file order) hold non-overlapping,
    // ascending ledger-seq ranges. Binary search over that sorted file list —
    // probing footer key ranges lazily — maps each wanted ledger seq to the
    // one file that covers it in O(log n) opens, instead of degrading to the
    // complete-SST scan when the name-keyed tiers miss (ledger seqs and
    // commit-seq file names drift apart on any group-committed vault).
    let unresolved = unresolved_seqs(wanted, rows);
    if !unresolved.is_empty() {
        let started = Instant::now();
        let before = rows.len();
        let mut index = CommitOrderedLedgerIndex::build(ledger_dirs)?;
        for seq in &unresolved {
            if let Some(path) = index.resolve(*seq)? {
                let path = path.clone();
                index.files_opened += 1;
                let reader = SstReader::open(&path)?;
                if let Some(value) = reader.get(&ledger_key(*seq))? {
                    insert_ledger_bytes(rows, *seq, value)?;
                }
            }
        }
        trace.record(
            "commit_ordered",
            unresolved.len(),
            rows.len() - before,
            index.files_opened,
            started,
        );
    }

    // Tier 3 (`named_scan`): directory scan for multi-part SSTs whose file
    // name seq is a wanted seq (same name-domain premise as tier 1, catching
    // `-0001`.. parts tier 1's fixed suffixes miss).
    let unresolved = unresolved_seqs(wanted, rows);
    if !unresolved.is_empty() {
        let started = Instant::now();
        let before = rows.len();
        let candidates = named_ledger_sst_candidates(ledger_dirs, &unresolved)?;
        let files_opened = candidates.len();
        read_rows_from_candidate_level(candidates, &unresolved, rows)?;
        trace.record(
            "named_scan",
            unresolved.len(),
            rows.len() - before,
            files_opened,
            started,
        );
    }

    Ok(())
}

pub(super) fn read_complete_sst_ledger_rows(
    ledger_dirs: &[PathBuf],
    wanted: &BTreeSet<u64>,
    rows: &mut BTreeMap<u64, Vec<u8>>,
    trace: &mut LedgerPointReadTrace,
) -> CalyxResult<()> {
    // Final tier (`complete_scan`): the semantic source of truth — every
    // Ledger SST. This runs only after the WAL tail has been inspected, because
    // unflushed tail rows are expected to be absent from SSTs during startup.
    let unresolved = unresolved_seqs(wanted, rows);
    if unresolved.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let before = rows.len();
    let candidates = complete_ledger_sst_candidates(ledger_dirs, &unresolved)?;
    let files_opened = candidates.len();
    read_rows_from_candidate_level(candidates, &unresolved, rows)?;
    trace.record(
        "complete_scan",
        unresolved.len(),
        rows.len() - before,
        files_opened,
        started,
    );
    Ok(())
}

pub(super) fn unresolved_seqs(
    wanted: &BTreeSet<u64>,
    rows: &BTreeMap<u64, Vec<u8>>,
) -> BTreeSet<u64> {
    wanted
        .iter()
        .copied()
        .filter(|seq| !rows.contains_key(seq))
        .collect()
}

fn read_rows_from_candidate_level(
    candidates: Vec<PathBuf>,
    wanted: &BTreeSet<u64>,
    rows: &mut BTreeMap<u64, Vec<u8>>,
) -> CalyxResult<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let level = SstLevel::from_oldest_first_with_lookup(candidates)?;
    for seq in wanted {
        let key = ledger_key(*seq);
        for value in level.values_for_key(&key)? {
            insert_ledger_bytes(rows, *seq, value)?;
        }
    }
    Ok(())
}

fn probable_ledger_sst_candidates(
    ledger_dirs: &[PathBuf],
    wanted: &BTreeSet<u64>,
) -> CalyxResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for dir in ledger_dirs {
        for seq in wanted {
            push_ledger_sst_candidate(&dir.join(format!("{seq:020}.sst")), &mut files)?;
            push_ledger_sst_candidate(&dir.join(format!("{seq:020}-0000.sst")), &mut files)?;
        }
    }
    sorted_unique_paths(files)
}

fn named_ledger_sst_candidates(
    ledger_dirs: &[PathBuf],
    wanted: &BTreeSet<u64>,
) -> CalyxResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for dir in ledger_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir).map_err(|error| {
            CalyxError::disk_pressure(format!("read ledger CF dir {}: {error}", dir.display()))
        })? {
            let path = entry
                .map_err(|error| {
                    CalyxError::disk_pressure(format!("read ledger SST entry: {error}"))
                })?
                .path();
            let Some(name) = classify_sst(&path)? else {
                continue;
            };
            // Candidate-selection heuristic only: a miss falls through to the
            // complete tier and every hit is byte-verified, so treating a
            // legacy flush ordinal as a candidate seq costs at most a probe.
            let seq = match name {
                SstName::RouterLegacy { ordinal } => ordinal,
                SstName::Flush { watermark, .. } => watermark,
                SstName::DurableBatch { seq, .. } => seq,
                SstName::Compacted { .. } => continue,
            };
            if !wanted.contains(&seq) {
                continue;
            }
            let order = sst_order_key(&path)?.ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "classified ledger SST {} has no order key",
                    path.display()
                ))
            })?;
            files.push((order, path));
        }
    }
    sorted_unique_paths(files)
}

/// Commit-ordered durable-batch ledger SSTs with lazily-probed footer key
/// ranges: the binary-searchable ledger-seq -> file map behind the
/// `commit_ordered` tier (#1112). The same technique is proven in
/// `calyx-cli::provenance_read::LedgerProvenanceIndex`; this is the
/// storage-layer counterpart keyed by ledger seq instead of commit seq.
struct CommitOrderedLedgerIndex {
    files: Vec<IndexedLedgerFile>,
    files_opened: usize,
}

struct IndexedLedgerFile {
    path: PathBuf,
    /// Lazily-loaded (first_ledger_seq, last_ledger_seq) from the SST footer.
    range: Option<(u64, u64)>,
}

impl CommitOrderedLedgerIndex {
    fn build(ledger_dirs: &[PathBuf]) -> CalyxResult<Self> {
        let mut files = Vec::new();
        for dir in ledger_dirs {
            if !dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(dir).map_err(|error| {
                CalyxError::disk_pressure(format!("read ledger CF dir {}: {error}", dir.display()))
            })? {
                let path = entry
                    .map_err(|error| {
                        CalyxError::disk_pressure(format!("read ledger SST entry: {error}"))
                    })?
                    .path();
                let Some(name) = classify_sst(&path)? else {
                    continue;
                };
                // Router flushes span many commit batches (overlapping seq
                // ranges) and compaction outputs rewrite row history: neither
                // participates in the ordered-range bisection. Rows only
                // reachable through them fall through to the later tiers,
                // which stay authoritative.
                if !matches!(name, SstName::DurableBatch { .. }) {
                    continue;
                }
                let order = sst_order_key(&path)?.ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "classified ledger SST {} has no order key",
                        path.display()
                    ))
                })?;
                files.push((order, path));
            }
        }
        let paths = sorted_unique_paths(files)?;
        Ok(Self {
            files: paths
                .into_iter()
                .map(|path| IndexedLedgerFile { path, range: None })
                .collect(),
            files_opened: 0,
        })
    }

    /// Maps a ledger seq to the durable-batch SST whose footer key range
    /// covers it, or `None` when no file covers the seq (the caller falls
    /// through to the remaining tiers). Correctness never depends on the
    /// ranges being monotone: a bisection miss only costs the fallback, and
    /// the row itself is read (and byte-verified against duplicates) from the
    /// actual file.
    fn resolve(&mut self, seq: u64) -> CalyxResult<Option<&PathBuf>> {
        let mut lo = 0usize;
        let mut hi = self.files.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (first, last) = self.range(mid)?;
            if seq < first {
                hi = mid;
            } else if seq > last {
                lo = mid + 1;
            } else {
                return Ok(Some(&self.files[mid].path));
            }
        }
        Ok(None)
    }

    fn range(&mut self, index: usize) -> CalyxResult<(u64, u64)> {
        if let Some(range) = self.files[index].range {
            return Ok(range);
        }
        let path = self.files[index].path.clone();
        self.files_opened += 1;
        let lookup = ledger_sst_lookup_metadata(&path)?;
        let range = (
            parse_aster_ledger_seq(&lookup.first_key)?,
            parse_aster_ledger_seq(&lookup.last_key)?,
        );
        self.files[index].range = Some(range);
        Ok(range)
    }
}

fn ledger_sst_lookup_metadata(path: &Path) -> CalyxResult<SstLookupMetadata> {
    SstReader::open(path)?.lookup_metadata().ok_or_else(|| {
        CalyxError::aster_corrupt_shard(format!("ledger SST {} has no keys", path.display()))
    })
}

fn sorted_unique_paths(
    mut files: Vec<(crate::storage_names::SstOrderKey, PathBuf)>,
) -> CalyxResult<Vec<PathBuf>> {
    files.sort_by(|(left_order, left_path), (right_order, right_path)| {
        left_order
            .cmp(right_order)
            .then_with(|| left_path.cmp(right_path))
    });
    let mut paths = files.into_iter().map(|(_, path)| path).collect::<Vec<_>>();
    paths.dedup();
    Ok(paths)
}

fn complete_ledger_sst_candidates(
    ledger_dirs: &[PathBuf],
    _wanted: &BTreeSet<u64>,
) -> CalyxResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for dir in ledger_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir).map_err(|error| {
            CalyxError::disk_pressure(format!("read ledger CF dir {}: {error}", dir.display()))
        })? {
            let path = entry
                .map_err(|error| {
                    CalyxError::disk_pressure(format!("read ledger SST entry: {error}"))
                })?
                .path();
            let Some(_) = classify_sst(&path)? else {
                continue;
            };
            let order = sst_order_key(&path)?.ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "classified ledger SST {} has no order key",
                    path.display()
                ))
            })?;
            files.push((order, path));
        }
    }
    sorted_unique_paths(files)
}

fn push_ledger_sst_candidate(
    path: &Path,
    files: &mut Vec<(crate::storage_names::SstOrderKey, PathBuf)>,
) -> CalyxResult<()> {
    if !path
        .try_exists()
        .map_err(|error| CalyxError::disk_pressure(format!("stat {}: {error}", path.display())))?
    {
        return Ok(());
    }
    let Some(name) = classify_sst(path)? else {
        return Ok(());
    };
    match name {
        SstName::RouterLegacy { .. } | SstName::Flush { .. } | SstName::DurableBatch { .. } => {}
        SstName::Compacted { .. } => {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "targeted ledger point read reached compacted ledger SST {}; add a compacted ledger row index before using point verification on compacted ledger layouts",
                path.display()
            )));
        }
    }
    let order = sst_order_key(path)?.ok_or_else(|| {
        CalyxError::aster_corrupt_shard(format!(
            "classified ledger SST {} has no order key",
            path.display()
        ))
    })?;
    files.push((order, path.to_path_buf()));
    Ok(())
}
