use super::{AsterVault, DEFAULT_LEASE_MS};
use crate::mvcc::{Freshness, Snapshot, VersionedCfStore};
use calyx_core::{Clock, Seq};

pub(crate) struct ScopedSnapshot<'a> {
    rows: &'a VersionedCfStore,
    snapshot: Snapshot,
}

impl ScopedSnapshot<'_> {
    pub(crate) const fn snapshot(&self) -> Snapshot {
        self.snapshot
    }
}

impl Drop for ScopedSnapshot<'_> {
    fn drop(&mut self) {
        self.rows.release_lease(self.snapshot.lease().id());
    }
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Runs one read operation against a single latest snapshot whose reader
    /// lease remains registered for the entire closure.
    ///
    /// The scoped handle releases the lease on success, error, or unwind. This
    /// is the composition boundary for callers that must scan candidate ids and
    /// subsequently hydrate those ids from the exact same MVCC view: passing a
    /// numeric sequence between separately pinned reads leaves a gap in which
    /// version GC can advance, while taking a fresh implicit snapshot for each
    /// read can mix committed states.
    pub fn with_scoped_latest_snapshot<T, E>(
        &self,
        freshness: Freshness,
        max_age_ms: u64,
        map_pin_error: impl FnOnce(calyx_core::CalyxError) -> E,
        read: impl FnOnce(Snapshot) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let snapshot = self
            .rows
            .pin_snapshot(freshness, &self.clock, max_age_ms)
            .map_err(map_pin_error)?;
        let snapshot = ScopedSnapshot {
            rows: &self.rows,
            snapshot,
        };
        read(snapshot.snapshot())
    }

    /// Runs one panel-scoped read against an atomic `(seq, panel watermark)`
    /// snapshot and releases the reader lease on every exit path.
    pub fn with_scoped_latest_snapshot_for_panel<T, E>(
        &self,
        panel_version: u32,
        freshness: Freshness,
        max_age_ms: u64,
        map_pin_error: impl FnOnce(calyx_core::CalyxError) -> E,
        read: impl FnOnce(Snapshot) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let snapshot = self
            .rows
            .pin_snapshot_for_panel(panel_version, freshness, &self.clock, max_age_ms)
            .map_err(map_pin_error)?;
        let snapshot = ScopedSnapshot {
            rows: &self.rows,
            snapshot,
        };
        read(snapshot.snapshot())
    }

    pub(crate) fn snapshot_handle(&self, seq: Seq) -> calyx_core::Result<ScopedSnapshot<'_>> {
        self.snapshot_handle_with_max_age(seq, DEFAULT_LEASE_MS)
    }

    pub(crate) fn snapshot_handle_with_max_age(
        &self,
        seq: Seq,
        max_age_ms: u64,
    ) -> calyx_core::Result<ScopedSnapshot<'_>> {
        let snapshot =
            self.rows
                .pin_snapshot_at(seq, Freshness::FreshDerived, &self.clock, max_age_ms)?;
        Ok(ScopedSnapshot {
            rows: &self.rows,
            snapshot,
        })
    }

    pub(crate) fn with_scoped_snapshot<T>(
        &self,
        seq: Seq,
        read: impl FnOnce(Snapshot) -> calyx_core::Result<T>,
    ) -> calyx_core::Result<T> {
        let snapshot = self.snapshot_handle(seq)?;
        read(snapshot.snapshot())
    }

    /// Runs one external read operation against an exact historical sequence
    /// while retaining and automatically releasing the reader lease.
    ///
    /// This is the historical counterpart of
    /// [`Self::with_scoped_latest_snapshot`]. It exists for bounded delta
    /// consumers that must enumerate changes through sequence `seq` and then
    /// hydrate those identities from that same physical view; composing a
    /// numeric sequence with separately pinned reads would leave a GC race.
    pub fn with_scoped_snapshot_at<T, E>(
        &self,
        seq: Seq,
        max_age_ms: u64,
        map_pin_error: impl FnOnce(calyx_core::CalyxError) -> E,
        read: impl FnOnce(Snapshot) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let snapshot = self
            .rows
            .pin_snapshot_at(seq, Freshness::FreshDerived, &self.clock, max_age_ms)
            .map_err(map_pin_error)?;
        let snapshot = ScopedSnapshot {
            rows: &self.rows,
            snapshot,
        };
        read(snapshot.snapshot())
    }
}
