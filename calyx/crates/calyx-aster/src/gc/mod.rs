//! Garbage-collection and reclaimer scaffolding for Aster.

pub mod ann_gc;
pub mod compaction_gc;
pub mod orphan_reconciler;
pub mod panel_version_gc;
pub mod snapshot_gc;
pub mod wal_recycler;

pub use ann_gc::{
    AnnGcReclaimer, AnnGcResult, AnnGcTarget, AnnIndexGraph, AnnTombstoneStats, CALYX_IO_ERROR,
    DEFAULT_ANN_MAX_SERVING_IO_LOAD, DEFAULT_ANN_MAX_TOMBSTONE_RATIO,
    DEFAULT_ANN_REBUILD_INTERVAL_MS, SharedAnnIndex, ann_io_error,
};
pub use compaction_gc::{
    CompactionCadence, CompactionGcReclaimer, CompactionGcResult, CompactionGcTarget,
    CompactionIoStats, CompactionThrottle, DEFAULT_COMPACTION_DEBT_ALERT_THRESHOLD,
    DEFAULT_DISK_BW_BYTES_PER_SEC, DEFAULT_MAX_IO_FRACTION, DEFAULT_TOMBSTONE_RATIO_TRIGGER,
    TombstoneCfStats, TombstoneInventory, VaultCompactionGcTarget, scan_catalog_tombstones,
    scan_tombstone_inventory, tombstone_ratio_for_counts,
};
pub use orphan_reconciler::{
    OrphanBaseEntry, OrphanGcTarget, OrphanIndexEntry, OrphanReconciler, OrphanRepairResult,
    OrphanReport, VaultOrphanGcTarget,
};
pub use panel_version_gc::{
    CodebookVersionGc, CodebookVersionGcTarget, PanelVersionGc, PanelVersionGcResult,
    PanelVersionGcTarget, PanelVersionId, PanelVersionRecord, RetentionPolicy, RetiredLensGc,
    RetiredLensGcTarget, VaultPanelVersionGcTarget, VersionTier,
};
pub use snapshot_gc::{
    BoundedStalenessSnapshot, CALYX_GC_ERROR, DEFAULT_GC_MAX_OPS_PER_RUN,
    DEFAULT_GC_MIN_INTERVAL_MS, DEFAULT_MAX_PINNED_SEQ_GAP, DEFAULT_READER_LEASE_MS, GapAlert,
    GcMetrics, GcRateLimit, GcResult, GcScheduler, GcSchedulerTick, GcTask, ReadLease,
    ReadLeaseRenewal, ReaderId, SnapshotGcCounters, SnapshotGcReclaimer, SnapshotGcTick,
    SnapshotPinMetrics, SnapshotPinWatchdog, SnapshotVersionGc, gc_error,
};
pub use wal_recycler::{
    DEFAULT_FSYNC_BUDGET_PER_TICK, DEFAULT_FSYNC_P99_ALERT_US, DEFAULT_MAX_RECYCLE_PER_TICK,
    DEFAULT_WAL_RECYCLER_MIN_INTERVAL_MS, WalRecycler, WalRecyclerResult,
};
