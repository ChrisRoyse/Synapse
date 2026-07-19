//! Snapshot-safe SST compaction and hot/cold tier placement.

mod rolling;
mod scan;
mod scheduler;
mod tiering;

use crate::cf::ColumnFamily;
use crate::sst::shared_reader;
use crate::storage_names::{SstName, classify_sst, sst_order_key};
use calyx_core::{CalyxError, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Default per-CF compaction target used for debt scoring (PRD 24 §8).
pub const DEFAULT_COMPACTION_TARGET_BYTES: u64 = 64 * 1024 * 1024;
/// File-count ceiling used for live fan-out admission. Tiny commit SSTs can
/// create severe read/recovery amplification while remaining far below the
/// per-output byte target; total logical CF bytes are not themselves
/// reclaimable in a flat immutable-file layout.
pub const DEFAULT_COMPACTION_TARGET_FILES: usize = 256;
const WRITE_AMP_SCALE: u64 = 1_000;
const COMPACTION_ADOPTION_FIRST_INDEX: usize = 9_000;
const COMPACTION_ADOPTION_LAST_INDEX: usize = 9_999;

pub(crate) use rolling::RollingSstWriter;
use rolling::compact_shards_with_target;
pub(crate) use scan::catalog_from_vault_tiers_through_seq;
pub use scan::{catalog_from_vault_dir, catalog_from_vault_tiers};
pub use scheduler::{
    AdaptiveCompactionSchedule, CompactionScheduleDecision, CompactionScheduleHook,
    CompactionScheduleState, CompactionScheduler, CompactionSchedulerHealth,
    CompactionSchedulerOptions,
};
pub use tiering::{StorageTier, TierPlacement, TierWrite, TieringPolicy};

/// One immutable SST file in the active shard set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstShard {
    pub cf: ColumnFamily,
    pub path: PathBuf,
    pub level: u8,
    pub bytes: u64,
}

impl SstShard {
    pub fn new(cf: ColumnFamily, path: impl AsRef<Path>, level: u8) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = fs::metadata(&path)
            .map_err(|error| CalyxError::disk_pressure(format!("stat SST shard: {error}")))?
            .len();
        Ok(Self {
            cf,
            path,
            level,
            bytes,
        })
    }
}

/// Pinned view of the active shard set. Old views survive compaction swaps.
#[derive(Debug, Clone)]
pub struct CompactionSnapshot {
    shards: Arc<Vec<SstShard>>,
}

impl CompactionSnapshot {
    pub fn get(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        for shard in self.shards.iter().rev().filter(|shard| shard.cf == cf) {
            if let Some(value) = shared_reader(&shard.path)?.get(key)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn shard_count_for_cf(&self, cf: ColumnFamily) -> usize {
        self.shards.iter().filter(|shard| shard.cf == cf).count()
    }
}

/// Active SST catalog with atomic snapshot swaps.
#[derive(Debug)]
pub struct CompactionCatalog {
    active: RwLock<Arc<Vec<SstShard>>>,
}

impl CompactionCatalog {
    pub fn new(shards: Vec<SstShard>) -> Self {
        Self {
            active: RwLock::new(Arc::new(shards)),
        }
    }

    pub fn pin_snapshot(&self) -> CompactionSnapshot {
        CompactionSnapshot {
            shards: self.active.read().expect("catalog lock").clone(),
        }
    }

    pub fn compact_cf(
        &self,
        cf: ColumnFamily,
        output_path: impl AsRef<Path>,
        throttle: CompactionThrottle,
    ) -> Result<CompactionResult> {
        let before = self.pin_snapshot();
        let inputs: Vec<_> = before
            .shards
            .iter()
            .filter(|shard| shard.cf == cf)
            .cloned()
            .collect();
        let CompactionResult::Compacted(report) =
            compact_shards(cf, &inputs, output_path, throttle)?
        else {
            return Ok(CompactionResult::Skipped {
                debt: CompactionDebt::measure(&inputs, DEFAULT_COMPACTION_TARGET_BYTES),
            });
        };

        let next_level = inputs.iter().map(|shard| shard.level).max().unwrap_or(0) + 1;
        let compacted = report
            .output_paths
            .iter()
            .map(|path| SstShard::new(cf, path, next_level))
            .collect::<Result<Vec<_>>>()?;
        let mut next: Vec<_> = self
            .active
            .read()
            .expect("catalog lock")
            .iter()
            .filter(|shard| shard.cf != cf)
            .cloned()
            .collect();
        next.extend(compacted);
        *self.active.write().expect("catalog lock") = Arc::new(next);
        Ok(CompactionResult::Compacted(report))
    }

    /// Compacts at most the oldest `max_input_files` for one CF.
    ///
    /// Selecting an oldest contiguous prefix preserves newest-wins ordering
    /// when newer shards remain live. The caller must name the output from the
    /// selected inputs' commit domain and reclaim only `report.input_paths`.
    pub fn compact_cf_oldest_files(
        &self,
        cf: ColumnFamily,
        output_path: impl AsRef<Path>,
        throttle: CompactionThrottle,
        max_input_files: usize,
    ) -> Result<CompactionResult> {
        self.compact_cf_file_range(cf, output_path, throttle, 0, max_input_files)
    }

    /// Compacts one contiguous range in canonical oldest-to-newest order.
    ///
    /// A range output must be named in the maximum commit domain represented
    /// by that range. That leaves every unselected newer SST later in the
    /// canonical order and therefore preserves newest-wins reads.
    pub fn compact_cf_file_range(
        &self,
        cf: ColumnFamily,
        output_path: impl AsRef<Path>,
        throttle: CompactionThrottle,
        start_input_file: usize,
        input_files: usize,
    ) -> Result<CompactionResult> {
        if input_files < 2 {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "bounded compaction requires input_files >= 2, got {input_files}"
            )));
        }
        let before = self.pin_snapshot();
        let all_inputs = before
            .shards
            .iter()
            .filter(|shard| shard.cf == cf)
            .cloned()
            .collect::<Vec<_>>();
        let end_input_file = start_input_file.checked_add(input_files).ok_or_else(|| {
            CalyxError::aster_corrupt_shard("bounded compaction input range overflow")
        })?;
        if end_input_file > all_inputs.len() {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "bounded compaction input range {start_input_file}..{end_input_file} exceeds {} files for {}",
                all_inputs.len(),
                cf.name()
            )));
        }
        let inputs = all_inputs[start_input_file..end_input_file].to_vec();
        let CompactionResult::Compacted(report) =
            compact_shards(cf, &inputs, output_path, throttle)?
        else {
            return Ok(CompactionResult::Skipped {
                debt: CompactionDebt::measure(&inputs, DEFAULT_COMPACTION_TARGET_BYTES),
            });
        };

        let next_level = inputs.iter().map(|shard| shard.level).max().unwrap_or(0) + 1;
        let compacted = report
            .output_paths
            .iter()
            .map(|path| SstShard::new(cf, path, next_level))
            .collect::<Result<Vec<_>>>()?;
        let compacted_inputs = inputs
            .iter()
            .map(|shard| shard.path.clone())
            .collect::<BTreeSet<_>>();
        let mut next = self
            .active
            .read()
            .expect("catalog lock")
            .iter()
            .filter(|shard| !compacted_inputs.contains(&shard.path))
            .cloned()
            .collect::<Vec<_>>();
        next.extend(compacted);
        next.sort_by(|left, right| {
            left.cf
                .name()
                .cmp(&right.cf.name())
                .then_with(|| {
                    canonical_sst_order(&left.path).cmp(&canonical_sst_order(&right.path))
                })
                .then_with(|| left.path.cmp(&right.path))
        });
        *self.active.write().expect("catalog lock") = Arc::new(next);
        Ok(CompactionResult::Compacted(report))
    }

    pub fn shard_count_for_cf(&self, cf: ColumnFamily) -> usize {
        self.pin_snapshot().shard_count_for_cf(cf)
    }

    pub fn shards_for_cf(&self, cf: ColumnFamily) -> Vec<SstShard> {
        self.pin_snapshot()
            .shards
            .iter()
            .filter(|shard| shard.cf == cf)
            .cloned()
            .collect()
    }

    pub fn debt_for_cf(&self, cf: ColumnFamily, target_bytes: u64) -> CompactionDebt {
        let snapshot = self.pin_snapshot();
        let inputs: Vec<_> = snapshot
            .shards
            .iter()
            .filter(|shard| shard.cf == cf)
            .cloned()
            .collect();
        CompactionDebt::measure(&inputs, target_bytes)
    }

    pub fn column_families(&self) -> Vec<ColumnFamily> {
        let snapshot = self.pin_snapshot();
        let mut cfs = Vec::new();
        for shard in snapshot.shards.iter() {
            if !cfs.contains(&shard.cf) {
                cfs.push(shard.cf);
            }
        }
        cfs
    }
}

/// Commit-domain output path for a compaction of `inputs` (issue #1137).
///
/// The output is named from the highest commit-domain seq present in the
/// inputs — the highest seq at which the merged state is true — so
/// full-restore readback restores the merged rows at that seq instead of
/// misreading a foreign counter as commit seq ~1 and corrupting history.
/// Legacy router flushes carry no commit-domain bound and contribute seq 0;
/// their content stays sound under the durable-coverage invariant plus the
/// `ensure_unambiguous_sst_order` gate (issue #1138). Fails closed on
/// non-canonical input names and when no adoption slot remains.
pub fn commit_domain_output_path(dir: &Path, inputs: &[SstShard]) -> Result<PathBuf> {
    let mut max_seq = 0_u64;
    for shard in inputs {
        let name = classify_sst(&shard.path)?.ok_or_else(|| {
            CalyxError::aster_corrupt_shard(format!(
                "compaction input {} is not an SST file",
                shard.path.display()
            ))
        })?;
        let seq = match name {
            SstName::RouterLegacy { .. } => 0,
            SstName::Flush { watermark, .. } => watermark,
            SstName::DurableBatch { seq, .. } | SstName::Compacted { seq } => seq,
        };
        max_seq = max_seq.max(seq);
    }
    durable_compaction_slot_path(dir, max_seq)
}

/// First free `{seq:020}-{index:04}.sst` adoption slot in the reserved
/// `9000..=9999` index range (descending), the durable-batch-shaped naming
/// full-restore readback restores at `seq`. Shared by the CLI `compact`
/// command and the background scheduler so every commit-domain compaction
/// output is named through one implementation.
pub fn durable_compaction_slot_path(dir: &Path, seq: u64) -> Result<PathBuf> {
    for index in (COMPACTION_ADOPTION_FIRST_INDEX..=COMPACTION_ADOPTION_LAST_INDEX).rev() {
        let path = dir.join(format!("{seq:020}-{index:04}.sst"));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(CalyxError {
        code: "CALYX_ASTER_COMPACTION_SLOTS_EXHAUSTED",
        message: format!(
            "no compaction adoption slot ({}..={}) remains for commit seq {seq} in {}",
            COMPACTION_ADOPTION_FIRST_INDEX,
            COMPACTION_ADOPTION_LAST_INDEX,
            dir.display()
        ),
        remediation: "advance the vault's durable seq (write and flush) so compaction outputs \
                      move to a new seq, or remove superseded adoption slots via a full \
                      compaction, then retry",
    })
}

/// Per-run throttle. `None` means no byte cap for the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionThrottle {
    pub max_input_bytes: Option<u64>,
}

impl CompactionThrottle {
    pub const fn unlimited() -> Self {
        Self {
            max_input_bytes: None,
        }
    }

    pub const fn max_input_bytes(max_input_bytes: u64) -> Self {
        Self {
            max_input_bytes: Some(max_input_bytes),
        }
    }
}

/// Compaction debt meter for anti-storm scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionDebt {
    pub pending_bytes: u64,
    pub target_bytes: u64,
    pub pending_files: usize,
    pub target_files: usize,
    pub byte_score_milli: u64,
    pub file_score_milli: u64,
    pub score_milli: u64,
}

impl CompactionDebt {
    pub fn measure(shards: &[SstShard], target_bytes: u64) -> Self {
        let pending_bytes: u64 = shards.iter().map(|shard| shard.bytes).sum();
        let target_bytes = target_bytes.max(1);
        let pending_files = shards.len();
        let target_files = DEFAULT_COMPACTION_TARGET_FILES.max(1);
        // `target_bytes` is the desired size of one rolled output SST, not a
        // cap on the total logical size of a CF. Total bytes alone are not
        // reclaimable debt in this flat immutable-file layout: repeatedly
        // rewriting four healthy 64 MiB files into four new 64 MiB files
        // changes no physical state. Keep the raw byte ratio for telemetry,
        // but admit maintenance only on file fan-out.
        let byte_score_milli = pending_bytes.saturating_mul(WRITE_AMP_SCALE) / target_bytes;
        let file_score_milli = u64::try_from(pending_files)
            .unwrap_or(u64::MAX)
            .saturating_mul(WRITE_AMP_SCALE)
            / u64::try_from(target_files).unwrap_or(u64::MAX);
        Self {
            pending_bytes,
            target_bytes,
            pending_files,
            target_files,
            byte_score_milli,
            file_score_milli,
            score_milli: file_score_milli,
        }
    }
}

fn canonical_sst_order(path: &Path) -> crate::storage_names::SstOrderKey {
    sst_order_key(path)
        .expect("compaction catalog contains only validated SST paths")
        .expect("compaction catalog path has an SST order")
}

/// Result of one compaction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionResult {
    Skipped { debt: CompactionDebt },
    Compacted(Box<CompactionReport>),
}

/// Physical compaction metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    pub cf: ColumnFamily,
    pub input_files: usize,
    pub input_paths: Vec<PathBuf>,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub logical_bytes: u64,
    pub write_amp_milli: u64,
    pub reclaimed_input_files: usize,
    pub debt_before: CompactionDebt,
    pub debt_after: CompactionDebt,
    pub output_path: PathBuf,
    pub output_paths: Vec<PathBuf>,
    pub staging_parent: PathBuf,
}

pub fn compact_shards(
    cf: ColumnFamily,
    inputs: &[SstShard],
    output_path: impl AsRef<Path>,
    throttle: CompactionThrottle,
) -> Result<CompactionResult> {
    compact_shards_with_target(
        cf,
        inputs,
        output_path,
        throttle,
        DEFAULT_COMPACTION_TARGET_BYTES,
    )
}
