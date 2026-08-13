//! Vault-wide MVCC sequence and snapshot scaffolding.

mod lease;
mod read_barrier;
mod store;

pub use lease::{Freshness, ReaderLease, SeqAllocator, Snapshot};
pub use read_barrier::{CALYX_ASTER_BASE_CORRUPT, ReadBarrier};
pub(crate) use store::TOMBSTONE_VALUE;
pub use store::{
    CfChangeSignal, CfRead, DEFAULT_SNAPSHOT_VERSION_GC_MAX_CHAINS,
    DEFAULT_SNAPSHOT_VERSION_GC_MAX_PASS_US, DEFAULT_SNAPSHOT_VERSION_GC_MAX_SHARD_HOLD_US,
    DEFAULT_SNAPSHOT_VERSION_GC_MAX_VERSIONS, FlushStatus, LATEST_CF_RANGE_PAGE_MAX_ROWS,
    LatestCfRangePage, MVCC_STAGE_COUNT, MVCC_STAGE_NAMES, MvccCommitTimings, MvccResidentStatus,
    PanelScopedChangedKeys, ROW_READ_GUARD_WARN_US, RowGuardSite, RowGuardSiteCensus,
    SnapshotVersionGcBudget, SnapshotVersionGcPass, SnapshotVersionGcStop, VersionedCfStore,
    is_tombstone_value, tombstone_value,
};
