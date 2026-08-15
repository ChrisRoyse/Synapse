//! Aggregate `resource_status` surface (PRD 18 §4, 24 §8; issue #592).
//!
//! One call returns the resource health an operator or Anneal acts on:
//! heap RSS, VRAM budget use, compaction debt, oldest-pinned-seq gap, and
//! backpressure events — each read from its physical source of truth.

mod collect;
mod counters;
mod heap;
mod leases;
mod status;

pub use collect::collect_resource_status;
pub use counters::{BackpressureStatus, ResourceCounters};
pub use heap::{CALYX_RESOURCE_PROBE_UNAVAILABLE, heap_rss_bytes, process_private_bytes};
pub(crate) use leases::ReaderLeaseRenewal;
pub use leases::{LeaseRegistry, LeaseView};
pub use status::{
    CfCompactionDebt, CompactionDebtStatus, HeapStatus, MemtableCfStatus, MemtableStatus,
    PinnedSeqStatus, RESOURCE_STATUS_SCHEMA_VERSION, ResourceStatus, VramBudgetStatus, WalStatus,
};
