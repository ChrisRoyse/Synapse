use super::*;
use std::ops::ControlFlow;

/// Outcome of one allocation-reusing row walk through a pinned snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsterSnapshotCfRowWalk {
    /// Live rows lent to the visitor.
    pub rows_visited: usize,
    /// Whether the visitor ended the walk before cursor exhaustion.
    pub stopped_early: bool,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Reads exact physical slot-CF rows at one already-pinned snapshot.
    ///
    /// Callers choose the batch size.  Unlike a range scan, this never
    /// materializes unrelated large embedding values and preserves input
    /// order (including explicit `None` entries for missing rows).
    pub fn read_slot_cf_batch_snapshot(
        &self,
        snapshot: Snapshot,
        slot: calyx_core::SlotId,
        ids: &[CxId],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.assert_cf_selected(ColumnFamily::slot(slot), "read_slot_cf_batch_snapshot")?;
        let reads = ids
            .iter()
            .map(|id| crate::mvcc::CfRead::new(ColumnFamily::slot(slot), crate::cf::slot_key(*id)))
            .collect::<Vec<_>>();
        self.rows.read_batch(snapshot, &reads, &self.clock)
    }

    /// Streams visible raw CF rows at `snapshot` in bounded pages.
    pub fn scan_cf_pages_at<F, E>(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        limit: usize,
        on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        self.assert_cf_selected(cf, "scan_cf_pages_at")
            .map_err(E::from)?;
        let snapshot = self.snapshot_handle(snapshot)?;
        self.rows
            .scan_cf_pages_at(snapshot.snapshot(), cf, limit, &self.clock, on_page)
    }

    /// Streams the latest visible raw CF rows with one bounded lease per page.
    ///
    /// This contract is for cold physical readbacks that can legitimately run
    /// longer than the normal reader-lease window. It does not lengthen or
    /// disable that window: visible keys are selected once, and every bounded
    /// value page receives a fresh lease at the same sequence. The lease is
    /// released before the callback runs. If any writer advances the vault,
    /// the scan fails closed instead of mixing sequences or renewing a stale
    /// view whose versions could have become reclaimable between pages.
    pub fn scan_cf_pages_at_renewing_latest<F, E>(
        &self,
        snapshot: Seq,
        cf: ColumnFamily,
        limit: usize,
        mut on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        let ensure_latest = || -> std::result::Result<(), E> {
            let latest = self.latest_seq();
            if latest != snapshot {
                return Err(E::from(calyx_core::CalyxError::stale_derived(format!(
                    "renewing {} scan requires unchanged latest sequence {snapshot}, observed {latest}",
                    cf.name()
                ))));
            }
            Ok(())
        };

        self.assert_cf_selected(cf, "scan_cf_pages_at_renewing_latest")
            .map_err(E::from)?;
        ensure_latest()?;
        if limit == 0 {
            return Ok(());
        }
        let keys = self
            .scan_cf_range_keys_at(snapshot, cf, &KeyRange::all())
            .map_err(E::from)?;
        ensure_latest()?;

        for page_keys in keys.chunks(limit) {
            ensure_latest()?;
            let page = {
                let pinned = self.snapshot_handle(snapshot)?;
                let reads = page_keys
                    .iter()
                    .cloned()
                    .map(|key| crate::mvcc::CfRead::new(cf, key))
                    .collect::<Vec<_>>();
                let values = self
                    .rows
                    .read_batch(pinned.snapshot(), &reads, &self.clock)
                    .map_err(E::from)?;
                page_keys
                    .iter()
                    .cloned()
                    .zip(values)
                    .map(|(key, value)| {
                        value.map(|value| (key.clone(), value)).ok_or_else(|| {
                            let key_prefix = key
                                .iter()
                                .take(8)
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>();
                            // See the sibling message in
                            // `mvcc::store::read::scan_cf_range_page_at`: the
                            // renewing scan re-checks `ensure_latest()` around
                            // every page, so a changed sequence is reported as
                            // `stale_derived` and never reaches here. Arriving
                            // here means the key view and the value view
                            // disagree at one unchanged sequence, which is a
                            // deterministic disagreement rather than something
                            // "disappearing" mid-scan (#1954).
                            E::from(calyx_core::CalyxError::aster_corrupt_shard(format!(
                                "{} key {} was selected as visible at unchanged latest seq \
                                 {snapshot} by the key view (scan_cf_range_keys_at) but the \
                                 value view (read_batch) resolved no live value at that same \
                                 sequence; the two latest views disagree about this key's \
                                 visibility, which is deterministic and not a concurrent \
                                 mutation — the sequence was re-verified unchanged around \
                                 this page",
                                cf.name(),
                                key_prefix,
                            )))
                        })
                    })
                    .collect::<std::result::Result<Vec<_>, E>>()?
            };
            on_page(page)?;
            ensure_latest()?;
        }
        Ok(())
    }

    /// Streams visible raw CF rows using an already-pinned snapshot lease.
    pub fn scan_cf_pages_snapshot<F, E>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        limit: usize,
        on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        self.assert_cf_selected(cf, "scan_cf_pages_snapshot")
            .map_err(E::from)?;
        self.rows
            .scan_cf_pages_at(snapshot, cf, limit, &self.clock, on_page)
    }

    /// Counts visible raw rows at an already-pinned snapshot without opening
    /// immutable payloads. Returns `(rows, candidate_pages)`.
    pub fn count_cf_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        limit: usize,
    ) -> Result<(usize, usize)> {
        self.assert_cf_selected(cf, "count_cf_snapshot")?;
        self.rows.count_cf_at(snapshot, cf, limit, &self.clock)
    }

    /// Scans at most `limit` visible raw CF rows using an already-pinned snapshot lease.
    pub fn scan_cf_range_page_snapshot(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.assert_cf_selected(cf, "scan_cf_range_page_snapshot")?;
        self.rows
            .scan_cf_range_page_at(snapshot, cf, range, after_key, limit, &self.clock)
    }

    /// Walks visible rows through one allocation-reusing pinned-snapshot cursor.
    ///
    /// The visitor borrows each key/value only until it returns. Router-backed
    /// vaults retain one reusable value buffer per intersecting immutable
    /// source plus the bounded MVCC overlay; this method never constructs an
    /// output page or corpus-sized collection. The cursor revalidates the
    /// snapshot lease at its internal cadence and applies read barriers before
    /// lending a row. Visitor errors are returned unchanged through `E`.
    pub fn walk_cf_range_rows_snapshot<F, E>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        mut on_row: F,
    ) -> std::result::Result<AsterSnapshotCfRowWalk, E>
    where
        F: FnMut(&[u8], &[u8]) -> std::result::Result<ControlFlow<()>, E>,
        E: From<calyx_core::CalyxError>,
    {
        self.assert_cf_selected(cf, "walk_cf_range_rows_snapshot")
            .map_err(E::from)?;
        let mut stream = self
            .rows
            .open_cf_range_row_stream_at(snapshot, cf, range, &self.clock)
            .map_err(E::from)?;
        let mut rows_visited = 0_usize;
        loop {
            let mut visitor_result = None;
            let present = stream
                .next_with(|key, value| {
                    visitor_result = Some(on_row(key, value));
                    Ok(())
                })
                .map_err(E::from)?;
            if !present {
                return Ok(AsterSnapshotCfRowWalk {
                    rows_visited,
                    stopped_early: false,
                });
            }
            rows_visited = rows_visited.checked_add(1).ok_or_else(|| {
                E::from(calyx_core::CalyxError {
                    code: "CALYX_ASTER_SNAPSHOT_ROW_WALK_OVERFLOW",
                    message: format!(
                        "the {} snapshot row walk exceeded usize on this host",
                        cf.name()
                    ),
                    remediation: "inspect the immutable manifest and repair the impossible row cardinality before retrying",
                })
            })?;
            let control = visitor_result.take().ok_or_else(|| {
                E::from(calyx_core::CalyxError::aster_corrupt_shard(format!(
                    "the {} snapshot row cursor reported a present row without invoking its visitor",
                    cf.name()
                )))
            })??;
            match control {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => {
                    return Ok(AsterSnapshotCfRowWalk {
                        rows_visited,
                        stopped_early: true,
                    });
                }
            }
        }
    }

    /// Streams visible raw CF rows in bounded pages using an already-pinned snapshot lease.
    pub fn scan_cf_range_pages_snapshot<F, E>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        limit: usize,
        on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        self.assert_cf_selected(cf, "scan_cf_range_pages_snapshot")
            .map_err(E::from)?;
        self.rows
            .scan_cf_range_pages_at(snapshot, cf, range, limit, &self.clock, on_page)
    }
}
