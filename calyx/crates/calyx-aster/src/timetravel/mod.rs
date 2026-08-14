//! Time-travel reads: `as_of(t)` over MVCC time-keyed snapshots.
//!
//! `as_of` resolves a wall-clock timestamp to the greatest committed seqno at or
//! before it (via the [`time_index`] CF) and opens an MVCC snapshot pinned at
//! that seqno. The caller then reads any CF at that snapshot as if it were
//! `now`. The pin is released on [`Drop`] so version GC can proceed with no
//! leaked leases (A26). The time-index entry for each write is committed inside
//! the same WAL group-commit batch as the data (see `vault::commit`), so a crash
//! can never leave a write without its time mapping (A15).

mod retention;
mod time_index;

use calyx_core::{Clock, Constellation, CxId, Result, Seq, VaultStore};

use crate::cf::{ColumnFamily, KeyRange};
use crate::vault::AsterVault;

pub use retention::{
    CALYX_TIMETRAVEL_BEFORE_HORIZON, RetentionHorizon, before_horizon, check_horizon,
};
pub(crate) use time_index::entry_row;
pub use time_index::{TimeIndexEntry, read_all};

/// Lease age for a time-travel pin. Long enough for a historical read session;
/// the pin is released on drop regardless.
const TIMETRAVEL_LEASE_MS: u64 = 60_000;

/// A read handle pinned to a historical MVCC sequence resolved from a timestamp.
///
/// Holds a reader lease at `seqno` so version GC cannot reclaim the versions it
/// observes; the lease is released when the handle is dropped.
pub struct TimeTravelSnapshot<'a, C: Clock> {
    vault: &'a AsterVault<C>,
    seqno: Seq,
    resolved_at_millis: u64,
    lease_id: u64,
}

impl<'a, C: Clock> TimeTravelSnapshot<'a, C> {
    /// Opens a snapshot as of `t_millis`. Returns `CALYX_TIMETRAVEL_NO_DATA` if
    /// the vault has no write at or before `t`.
    pub fn open(vault: &'a AsterVault<C>, t_millis: u64) -> Result<Self> {
        let horizon = vault.retention_horizon();
        retention::check_horizon_at(&horizon, t_millis, vault.clock_now())?;
        let seqno = time_index::resolve(vault, t_millis)?;
        let lease_id = vault.pin_reader_at(seqno, TIMETRAVEL_LEASE_MS)?;
        Ok(Self {
            vault,
            seqno,
            resolved_at_millis: t_millis,
            lease_id,
        })
    }

    /// The MVCC sequence this snapshot reads at.
    pub fn seqno(&self) -> Seq {
        self.seqno
    }

    /// The timestamp this snapshot was resolved from.
    pub fn resolved_at_millis(&self) -> u64 {
        self.resolved_at_millis
    }

    /// Reads a constellation as it existed at this snapshot. A cx that had not
    /// been ingested by `seqno` is reported missing (it never silently returns
    /// a newer version).
    pub fn get_cx(&self, cx_id: CxId) -> Result<Constellation> {
        VaultStore::get(self.vault, cx_id, self.seqno)
    }

    /// Scans a CF over a key range at this snapshot.
    pub fn scan_cf(&self, cf: ColumnFamily, range: &KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.vault.scan_cf_range_at(self.seqno, cf, range)
    }

    /// Reads a single raw CF row at this snapshot.
    pub fn read_cf(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.vault.read_cf_at(self.seqno, cf, key)
    }
}

impl<C: Clock> std::fmt::Debug for TimeTravelSnapshot<'_, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeTravelSnapshot")
            .field("seqno", &self.seqno)
            .field("resolved_at_millis", &self.resolved_at_millis)
            .field("lease_id", &self.lease_id)
            .finish()
    }
}

impl<C: Clock> Drop for TimeTravelSnapshot<'_, C> {
    fn drop(&mut self) {
        // Release the GC pin. The lease may already be gone if it aged out; the
        // boolean result is advisory only.
        let _ = self.vault.release_reader(self.lease_id);
    }
}

/// Opens a [`TimeTravelSnapshot`] as of `t_millis` (free-function form of
/// [`TimeTravelSnapshot::open`]).
pub fn as_of<C: Clock>(vault: &AsterVault<C>, t_millis: u64) -> Result<TimeTravelSnapshot<'_, C>> {
    TimeTravelSnapshot::open(vault, t_millis)
}
