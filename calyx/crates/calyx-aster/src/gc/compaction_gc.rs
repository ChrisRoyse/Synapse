//! PH58 compaction GC facade for tombstone-heavy SST sets.

use crate::cf::ColumnFamily;
use crate::compaction::{CompactionCatalog, catalog_from_vault_dir};
use crate::mvcc::is_tombstone_value;
use crate::sst::shared_reader;
use crate::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Result};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const DEFAULT_MAX_IO_FRACTION: f64 = 0.20;
pub const DEFAULT_TOMBSTONE_RATIO_TRIGGER: f64 = 0.50;
pub const DEFAULT_COMPACTION_DEBT_ALERT_THRESHOLD: u64 = 64 * 1024 * 1024;
pub const DEFAULT_DISK_BW_BYTES_PER_SEC: u64 = 512 * 1024 * 1024;

const SKIP_LOW_TOMBSTONE_RATIO: &str = "tombstone_ratio_below_trigger";
const SKIP_LOW_IO_AVAILABLE: &str = "disk_io_available_below_max_fraction";
const SKIP_THROTTLED: &str = "compaction_throttle_empty";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactionIoStats {
    pub bytes_written_by_compaction: u64,
    pub bytes_written_by_flush: u64,
}

impl CompactionIoStats {
    pub fn write_amp(self) -> f64 {
        if self.bytes_written_by_flush == 0 {
            0.0
        } else {
            self.bytes_written_by_compaction as f64 / self.bytes_written_by_flush as f64
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TombstoneCfStats {
    pub cf: ColumnFamily,
    pub cf_name: String,
    pub sst_files: usize,
    pub sst_bytes: u64,
    pub live_keys: u64,
    pub tombstone_keys: u64,
    pub live_value_bytes: u64,
    pub tombstone_value_bytes: u64,
}

impl TombstoneCfStats {
    pub fn tombstone_ratio(&self) -> f64 {
        tombstone_ratio_for_counts(self.tombstone_keys, self.live_keys)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TombstoneInventory {
    pub per_cf: Vec<TombstoneCfStats>,
    pub io_stats: CompactionIoStats,
}

impl TombstoneInventory {
    pub fn tombstone_keys(&self) -> u64 {
        self.per_cf.iter().map(|cf| cf.tombstone_keys).sum()
    }

    pub fn live_keys(&self) -> u64 {
        self.per_cf.iter().map(|cf| cf.live_keys).sum()
    }

    pub fn tombstone_ratio(&self) -> f64 {
        tombstone_ratio_for_counts(self.tombstone_keys(), self.live_keys())
    }

    pub fn total_sst_bytes(&self) -> u64 {
        self.per_cf.iter().map(|cf| cf.sst_bytes).sum()
    }

    pub fn cfs_above_ratio(&self, threshold: f64) -> Vec<ColumnFamily> {
        self.per_cf
            .iter()
            .filter(|cf| cf.tombstone_ratio() > threshold)
            .map(|cf| cf.cf)
            .collect()
    }

    pub fn bytes_for_cfs(&self, cfs: &[ColumnFamily]) -> u64 {
        self.per_cf
            .iter()
            .filter(|cf| cfs.contains(&cf.cf))
            .map(|cf| cf.sst_bytes)
            .sum()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionGcResult {
    pub triggered: bool,
    pub rate_limited: bool,
    pub skipped_reason: Option<&'static str>,
    pub error_code: Option<&'static str>,
    pub error_message: Option<String>,
    pub tombstone_ratio_before: f64,
    pub tombstone_ratio_after: f64,
    pub bytes_compacted: u64,
    pub bytes_freed: u64,
    pub tombstones_removed: u64,
    pub write_amp_before: f64,
    pub write_amp_after: f64,
    pub compaction_debt: u64,
    pub compaction_debt_alert: bool,
    pub compacted_cfs: Vec<String>,
}

impl CompactionGcResult {
    pub fn to_metrics_text(&self, vault_label: &str) -> String {
        let vault = vault_label.replace('\\', "\\\\").replace('"', "\\\"");
        let mut out = String::new();
        let _ = writeln!(
            out,
            "calyx_tombstone_ratio{{vault=\"{vault}\"}} {:.6}",
            self.tombstone_ratio_after
        );
        let _ = writeln!(
            out,
            "calyx_write_amp{{vault=\"{vault}\"}} {:.6}",
            self.write_amp_after
        );
        let _ = writeln!(
            out,
            "calyx_compaction_debt{{vault=\"{vault}\"}} {}",
            self.compaction_debt
        );
        let _ = writeln!(
            out,
            "calyx_compaction_debt_alert{{vault=\"{vault}\"}} {}",
            u64::from(self.compaction_debt_alert)
        );
        out
    }

    fn skipped(
        before: &TombstoneInventory,
        reason: &'static str,
        rate_limited: bool,
        alert_threshold: u64,
    ) -> Self {
        let debt = before.total_sst_bytes();
        Self {
            triggered: false,
            rate_limited,
            skipped_reason: Some(reason),
            error_code: None,
            error_message: None,
            tombstone_ratio_before: before.tombstone_ratio(),
            tombstone_ratio_after: before.tombstone_ratio(),
            bytes_compacted: 0,
            bytes_freed: 0,
            tombstones_removed: 0,
            write_amp_before: before.io_stats.write_amp(),
            write_amp_after: before.io_stats.write_amp(),
            compaction_debt: debt,
            compaction_debt_alert: debt > alert_threshold,
            compacted_cfs: Vec::new(),
        }
    }

    fn error(before: Option<&TombstoneInventory>, error: CalyxError, alert_threshold: u64) -> Self {
        let debt = before.map_or(0, TombstoneInventory::total_sst_bytes);
        Self {
            triggered: false,
            rate_limited: false,
            skipped_reason: None,
            error_code: Some(error.code),
            error_message: Some(error.message),
            tombstone_ratio_before: before.map_or(0.0, TombstoneInventory::tombstone_ratio),
            tombstone_ratio_after: before.map_or(0.0, TombstoneInventory::tombstone_ratio),
            bytes_compacted: 0,
            bytes_freed: 0,
            tombstones_removed: 0,
            write_amp_before: before.map_or(0.0, |inventory| inventory.io_stats.write_amp()),
            write_amp_after: before.map_or(0.0, |inventory| inventory.io_stats.write_amp()),
            compaction_debt: debt,
            compaction_debt_alert: debt > alert_threshold,
            compacted_cfs: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionCadence {
    pub max_io_fraction: f64,
    pub compaction_debt: u64,
    pub debt_alert: bool,
}

#[derive(Debug)]
pub struct CompactionThrottle {
    state: Mutex<ThrottleState>,
    refill_bytes_per_sec: u64,
    capacity_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct ThrottleState {
    tokens: u64,
    last_refill_ms: u64,
}

impl CompactionThrottle {
    pub fn new(max_io_fraction: f64, disk_bw_bytes_per_sec: u64, now_ms: u64) -> Self {
        let refill = ((disk_bw_bytes_per_sec as f64) * max_io_fraction.clamp(0.0, 1.0)) as u64;
        let capacity = refill.max(1);
        Self {
            state: Mutex::new(ThrottleState {
                tokens: capacity,
                last_refill_ms: now_ms,
            }),
            refill_bytes_per_sec: capacity,
            capacity_bytes: capacity,
        }
    }

    pub fn try_consume(&self, bytes: u64, now_ms: u64) -> bool {
        let mut state = self.state.lock().expect("compaction throttle poisoned");
        let elapsed_ms = now_ms.saturating_sub(state.last_refill_ms);
        let refill = self.refill_bytes_per_sec.saturating_mul(elapsed_ms) / 1_000;
        state.tokens = state.tokens.saturating_add(refill).min(self.capacity_bytes);
        state.last_refill_ms = now_ms;
        if state.tokens < bytes {
            return false;
        }
        state.tokens -= bytes;
        true
    }
}

pub trait CompactionGcTarget {
    fn tombstone_inventory(&self) -> Result<TombstoneInventory>;
    fn compact_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()>;
}

pub struct VaultCompactionGcTarget<'a, C> {
    pub vault: &'a AsterVault<C>,
    pub vault_dir: &'a Path,
}

impl<C> CompactionGcTarget for VaultCompactionGcTarget<'_, C>
where
    C: Clock,
{
    fn tombstone_inventory(&self) -> Result<TombstoneInventory> {
        scan_tombstone_inventory(self.vault_dir)
    }

    fn compact_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()> {
        self.vault.purge_tombstoned_cfs(cfs)
    }
}

#[derive(Debug)]
pub struct CompactionGcReclaimer {
    pub max_io_fraction: f64,
    pub debt_metric: Arc<AtomicU64>,
    pub compaction_debt_alert_threshold: u64,
    pub tombstone_ratio_trigger: f64,
    throttle: CompactionThrottle,
}

impl CompactionGcReclaimer {
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_MAX_IO_FRACTION,
            DEFAULT_COMPACTION_DEBT_ALERT_THRESHOLD,
            DEFAULT_DISK_BW_BYTES_PER_SEC,
            0,
        )
    }

    pub fn with_limits(
        max_io_fraction: f64,
        compaction_debt_alert_threshold: u64,
        disk_bw_bytes_per_sec: u64,
        now_ms: u64,
    ) -> Self {
        Self {
            max_io_fraction,
            debt_metric: Arc::new(AtomicU64::new(0)),
            compaction_debt_alert_threshold,
            tombstone_ratio_trigger: DEFAULT_TOMBSTONE_RATIO_TRIGGER,
            throttle: CompactionThrottle::new(max_io_fraction, disk_bw_bytes_per_sec, now_ms),
        }
    }

    pub fn estimate_write_amp<T>(&self, target: &T) -> Result<f64>
    where
        T: CompactionGcTarget + ?Sized,
    {
        Ok(target.tombstone_inventory()?.io_stats.write_amp())
    }

    pub fn tombstone_ratio<T>(&self, target: &T) -> Result<f64>
    where
        T: CompactionGcTarget + ?Sized,
    {
        Ok(target.tombstone_inventory()?.tombstone_ratio())
    }

    pub fn adaptive_cadence(&self, serving_p99_below_slo: bool) -> CompactionCadence {
        let debt = self.debt_metric.load(Ordering::Relaxed);
        let debt_alert = debt > self.compaction_debt_alert_threshold;
        let max_io_fraction = if debt_alert && serving_p99_below_slo {
            (self.max_io_fraction * 1.5).min(1.0)
        } else {
            (self.max_io_fraction * 0.5).max(0.01)
        };
        CompactionCadence {
            max_io_fraction,
            compaction_debt: debt,
            debt_alert,
        }
    }

    pub fn maybe_trigger<T: CompactionGcTarget + ?Sized>(
        &self,
        target: &T,
        disk_io_available_fraction: f64,
    ) -> CompactionGcResult {
        self.maybe_trigger_at(target, disk_io_available_fraction, 0)
    }

    pub fn maybe_trigger_at<T>(
        &self,
        target: &T,
        disk_io_available_fraction: f64,
        now_ms: u64,
    ) -> CompactionGcResult
    where
        T: CompactionGcTarget + ?Sized,
    {
        let before = match target.tombstone_inventory() {
            Ok(inventory) => inventory,
            Err(error) => {
                return CompactionGcResult::error(
                    None,
                    error,
                    self.compaction_debt_alert_threshold,
                );
            }
        };
        let debt_before = before.total_sst_bytes();
        self.debt_metric.store(debt_before, Ordering::Relaxed);
        let cfs = before.cfs_above_ratio(self.tombstone_ratio_trigger);
        if cfs.is_empty() {
            return CompactionGcResult::skipped(
                &before,
                SKIP_LOW_TOMBSTONE_RATIO,
                false,
                self.compaction_debt_alert_threshold,
            );
        }
        if disk_io_available_fraction <= self.max_io_fraction {
            return CompactionGcResult::skipped(
                &before,
                SKIP_LOW_IO_AVAILABLE,
                false,
                self.compaction_debt_alert_threshold,
            );
        }
        let bytes_to_compact = before.bytes_for_cfs(&cfs);
        if !self.throttle.try_consume(bytes_to_compact.max(1), now_ms) {
            return CompactionGcResult::skipped(
                &before,
                SKIP_THROTTLED,
                true,
                self.compaction_debt_alert_threshold,
            );
        }
        if let Err(error) = target.compact_tombstoned_cfs(&cfs) {
            return CompactionGcResult::error(
                Some(&before),
                error,
                self.compaction_debt_alert_threshold,
            );
        }
        let after = match target.tombstone_inventory() {
            Ok(inventory) => inventory,
            Err(error) => {
                return CompactionGcResult::error(
                    Some(&before),
                    error,
                    self.compaction_debt_alert_threshold,
                );
            }
        };
        let debt_after = after.total_sst_bytes();
        self.debt_metric.store(debt_after, Ordering::Relaxed);
        CompactionGcResult {
            triggered: true,
            rate_limited: false,
            skipped_reason: None,
            error_code: None,
            error_message: None,
            tombstone_ratio_before: before.tombstone_ratio(),
            tombstone_ratio_after: after.tombstone_ratio(),
            bytes_compacted: bytes_to_compact,
            bytes_freed: debt_before.saturating_sub(debt_after),
            tombstones_removed: before
                .tombstone_keys()
                .saturating_sub(after.tombstone_keys()),
            write_amp_before: before.io_stats.write_amp(),
            write_amp_after: after.io_stats.write_amp(),
            compaction_debt: debt_after,
            compaction_debt_alert: debt_after > self.compaction_debt_alert_threshold,
            compacted_cfs: cfs.iter().map(ColumnFamily::name).collect(),
        }
    }
}

impl Default for CompactionGcReclaimer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn tombstone_ratio_for_counts(tombstones: u64, live: u64) -> f64 {
    let total = tombstones.saturating_add(live);
    if total == 0 {
        0.0
    } else {
        tombstones as f64 / total as f64
    }
}

pub fn scan_tombstone_inventory(vault_dir: &Path) -> Result<TombstoneInventory> {
    let catalog = catalog_from_vault_dir(vault_dir)?;
    scan_catalog_tombstones(&catalog)
}

pub fn scan_catalog_tombstones(catalog: &CompactionCatalog) -> Result<TombstoneInventory> {
    let mut per_cf = Vec::new();
    let mut io_stats = CompactionIoStats::default();
    for cf in catalog.column_families() {
        if cf == ColumnFamily::Ledger {
            continue;
        }
        let mut stats = TombstoneCfStats {
            cf,
            cf_name: cf.name(),
            sst_files: 0,
            sst_bytes: 0,
            live_keys: 0,
            tombstone_keys: 0,
            live_value_bytes: 0,
            tombstone_value_bytes: 0,
        };
        for shard in catalog.shards_for_cf(cf) {
            stats.sst_files += 1;
            stats.sst_bytes = stats.sst_bytes.saturating_add(shard.bytes);
            if is_compaction_output(&shard.path) {
                io_stats.bytes_written_by_compaction = io_stats
                    .bytes_written_by_compaction
                    .saturating_add(shard.bytes);
            } else {
                io_stats.bytes_written_by_flush =
                    io_stats.bytes_written_by_flush.saturating_add(shard.bytes);
            }
            for entry in shared_reader(&shard.path)?.iter()? {
                if is_tombstone_value(&entry.value) {
                    stats.tombstone_keys += 1;
                    stats.tombstone_value_bytes = stats
                        .tombstone_value_bytes
                        .saturating_add(entry.value.len() as u64);
                } else {
                    stats.live_keys += 1;
                    stats.live_value_bytes = stats
                        .live_value_bytes
                        .saturating_add(entry.value.len() as u64);
                }
            }
        }
        if stats.sst_files > 0 {
            per_cf.push(stats);
        }
    }
    per_cf.sort_by(|left, right| left.cf_name.cmp(&right.cf_name));
    Ok(TombstoneInventory { per_cf, io_stats })
}

fn is_compaction_output(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("compacted-"))
}
