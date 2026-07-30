//! Vault-wide MVCC sequence and snapshot scaffolding.

mod lease;
mod read_barrier;
mod store;

pub use lease::{Freshness, ReaderLease, SeqAllocator, Snapshot};
pub use read_barrier::{CALYX_ASTER_BASE_CORRUPT, ReadBarrier};
pub use store::{
    CfRead, LATEST_CF_RANGE_PAGE_MAX_ROWS, LatestCfRangePage, PanelScopedChangedKeys,
    VersionedCfStore, is_tombstone_value, tombstone_value,
};
