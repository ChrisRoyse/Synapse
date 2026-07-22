//! Aster storage engine skeleton for Calyx column families and WAL.

pub mod base_page_index;
pub mod cf;
pub mod collection;
pub mod compaction;
pub mod dedup;
pub mod erase;
mod file_lock;
mod fsync;
pub mod gc;
pub mod index;
pub mod layers;
pub mod ledger_head;
pub mod ledger_view;
pub mod manifest;
pub mod media_artifact;
pub mod memtable;
pub mod mmap_col;
pub mod mvcc;
pub mod olap;
pub mod plain_column;
pub mod plain_graph;
pub mod pressure;
pub mod recurrence;
pub mod redaction;
pub mod residency;
pub mod resource;
pub mod retained_input;
pub mod retention;
pub mod security;
pub mod sst;
pub mod storage_names;
pub mod stream;
pub mod supply_chain;
pub mod timetravel;
pub mod txn;
pub mod vault;
pub mod verify_restore;
pub mod wal;

pub use dedup::{
    CompressionRatio, Domain, DomainCompressionStats, compression_ratio, domain_compression_stats,
};

/// Highest slot id encodable by Aster's durable one-byte WAL CF tag.
///
/// Slot identifiers are durable schema identities. Callers must allocate only
/// within this inclusive bound and must not recycle retired identifiers.
pub const MAX_DURABLE_SLOT_ID: u16 = 47;

pub mod durable_fs {
    use std::path::Path;

    use calyx_core::Result;

    pub fn write_atomic_create_new(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
        crate::fsync::write_atomic_create_new(path, bytes, label)
    }

    pub fn write_atomic_replace(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
        crate::fsync::write_atomic_replace(path, bytes, label)
    }

    pub fn sync_parent(path: &Path, label: &str) -> Result<()> {
        crate::fsync::sync_parent(path, label)
    }
}
