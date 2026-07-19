//! Aggregate resource-health status (PRD 18 §4 `resource_status`, 24 §8).

use crate::gc::GcMetrics;
use crate::resource::counters::BackpressureStatus;
use calyx_core::Ts;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// Schema version for forward compatibility of persisted status documents.
pub const RESOURCE_STATUS_SCHEMA_VERSION: u32 = 1;

/// One-call aggregate of vault resource health (PRD 18 §4).
///
/// Field semantics follow PRD 24 §8: gauges describe the state at
/// `collected_at`; `*_total` counters are process-lifetime monotonic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceStatus {
    pub schema_version: u32,
    pub vault_dir: String,
    pub collected_at: Ts,
    pub heap: HeapStatus,
    pub memtable: MemtableStatus,
    pub vram: VramBudgetStatus,
    pub compaction: CompactionDebtStatus,
    pub gc: GcMetrics,
    pub pinned: PinnedSeqStatus,
    pub backpressure: BackpressureStatus,
    pub wal: WalStatus,
}

/// Process heap section, probed from `/proc/self/status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeapStatus {
    pub rss_bytes: u64,
}

/// Memtable byte-cap status across the currently open CF routers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemtableStatus {
    pub total_used_bytes: u64,
    pub total_cap_bytes: u64,
    pub per_cf: Vec<MemtableCfStatus>,
}

/// One column family's mutable memtable usage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemtableCfStatus {
    pub cf: String,
    pub used_bytes: u64,
    pub cap_bytes: u64,
    pub high_water_bytes: u64,
    pub flush_triggered: bool,
}

/// VRAM budget section, sourced from the vault Anneal budget enforcer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VramBudgetStatus {
    /// Configured VRAM budget from the vault `.anneal/budget.toml`.
    pub budget_bytes: u64,
    /// Sampled + reserved VRAM use reported by the budget enforcer.
    pub used_bytes: u64,
    /// Explicit probe degradation code (e.g. NVML unavailable); never silent.
    pub probe_warning: Option<String>,
}

/// Compaction debt for one column family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CfCompactionDebt {
    pub cf: String,
    pub sst_files: usize,
    pub pending_bytes: u64,
    pub byte_score_milli: u64,
    pub file_score_milli: u64,
    pub score_milli: u64,
}

/// Compaction debt section, measured from the on-disk SST shard set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionDebtStatus {
    pub target_bytes: u64,
    pub target_files: usize,
    pub total_pending_bytes: u64,
    pub total_pending_files: usize,
    pub max_score_milli: u64,
    pub per_cf: Vec<CfCompactionDebt>,
}

/// MVCC pinned-sequence section (PRD 24 §7 hazard row 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedSeqStatus {
    pub current_seq: u64,
    pub oldest_pinned_seq: Option<u64>,
    /// `current_seq - oldest_pinned_seq` across live leases; 0 when none.
    pub oldest_pinned_seq_gap: u64,
    pub active_leases: usize,
    pub reader_lease_expired_total: u64,
}

/// WAL footprint section, measured from `wal/*.wal` segment files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalStatus {
    pub segment_count: usize,
    pub bytes: u64,
}

impl ResourceStatus {
    /// Renders the status in Prometheus text exposition format.
    ///
    /// Metric names follow PRD 24 §8 and Prometheus naming conventions:
    /// snake_case, unit suffixes (`_bytes`), monotonic counters as `_total`.
    pub fn to_metrics_text(&self, vault_label: &str) -> String {
        let vault = escape_label(vault_label);
        let mut out = String::new();
        let mut metric = |name: &str, labels: String, value: u64| {
            let _ = writeln!(out, "{name}{{{labels}}} {value}");
        };
        let base = format!("vault=\"{vault}\"");
        metric("calyx_heap_rss_bytes", base.clone(), self.heap.rss_bytes);
        metric(
            "calyx_memtable_total_used_bytes",
            base.clone(),
            self.memtable.total_used_bytes,
        );
        metric(
            "calyx_memtable_total_cap_bytes",
            base.clone(),
            self.memtable.total_cap_bytes,
        );
        for cf in &self.memtable.per_cf {
            let labels = format!("{base},cf=\"{}\"", escape_label(&cf.cf));
            metric("calyx_memtable_used_bytes", labels.clone(), cf.used_bytes);
            metric("calyx_memtable_cap_bytes", labels.clone(), cf.cap_bytes);
            metric(
                "calyx_memtable_high_water_bytes",
                labels.clone(),
                cf.high_water_bytes,
            );
            metric(
                "calyx_memtable_flush_trigger",
                labels,
                u64::from(cf.flush_triggered),
            );
        }
        metric(
            "calyx_vram_budget_bytes",
            base.clone(),
            self.vram.budget_bytes,
        );
        metric("calyx_vram_used_bytes", base.clone(), self.vram.used_bytes);
        for cf in &self.compaction.per_cf {
            let labels = format!("{base},cf=\"{}\"", escape_label(&cf.cf));
            metric(
                "calyx_compaction_pending_compaction_bytes",
                labels.clone(),
                cf.pending_bytes,
            );
            metric(
                "calyx_compaction_pending_sst_files",
                labels.clone(),
                u64::try_from(cf.sst_files).unwrap_or(u64::MAX),
            );
            metric(
                "calyx_compaction_byte_debt_score_milli",
                labels.clone(),
                cf.byte_score_milli,
            );
            metric(
                "calyx_compaction_file_debt_score_milli",
                labels.clone(),
                cf.file_score_milli,
            );
            metric("calyx_compaction_debt_score_milli", labels, cf.score_milli);
        }
        metric(
            "calyx_compaction_target_bytes",
            base.clone(),
            self.compaction.target_bytes,
        );
        metric(
            "calyx_compaction_target_sst_files",
            base.clone(),
            u64::try_from(self.compaction.target_files).unwrap_or(u64::MAX),
        );
        metric(
            "calyx_gc_versions_reclaimed_total",
            base.clone(),
            self.gc.versions_reclaimed_total,
        );
        metric(
            "calyx_gc_bytes_freed_total",
            base.clone(),
            self.gc.bytes_freed_total,
        );
        metric(
            "calyx_gc_soft_deletes_purged_total",
            base.clone(),
            self.gc.soft_deletes_purged_total,
        );
        metric(
            "calyx_compaction_debt",
            base.clone(),
            self.gc.compaction_debt,
        );
        metric(
            "calyx_oldest_pinned_seq_gap",
            base.clone(),
            self.pinned.oldest_pinned_seq_gap,
        );
        metric(
            "calyx_active_reader_leases",
            base.clone(),
            self.pinned.active_leases as u64,
        );
        metric(
            "calyx_reader_lease_expired_total",
            base.clone(),
            self.pinned.reader_lease_expired_total,
        );
        metric(
            "calyx_backpressure_events_total",
            format!("{base},source=\"memtable_absorbed\""),
            self.backpressure.memtable_absorbed_total,
        );
        metric(
            "calyx_backpressure_events_total",
            format!("{base},source=\"memtable_rejected\""),
            self.backpressure.memtable_rejected_total,
        );
        metric(
            "calyx_disk_pressure_events_total",
            base.clone(),
            self.backpressure.disk_pressure_events_total,
        );
        metric("calyx_wal_bytes", base.clone(), self.wal.bytes);
        metric("calyx_wal_bytes_active", base.clone(), self.wal.bytes);
        metric("calyx_wal_segments", base, self.wal.segment_count as u64);
        out
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
