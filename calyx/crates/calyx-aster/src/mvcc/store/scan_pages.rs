use super::*;

/// Maximum transient copy of the post-recovery MVCC delta retained by one
/// pinned router-backed scan.
///
/// The immutable corpus is streamed and never counts here. Sixty-four MiB is
/// one normal Aster SST target, large enough for a meaningful changed-key
/// journal but small enough that a scan cannot duplicate an unbounded row
/// table and push a lightweight daemon into allocator failure. Exceeding this
/// limit is an explicit compaction/checkpoint debt error, never a request to
/// fall back to whole-range materialization.
const SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES: usize = 64 << 20;

impl VersionedCfStore {
    /// Streams visible rows for one CF at the pinned sequence in bounded pages.
    pub fn scan_cf_pages_at<F, E>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        limit: usize,
        clock: &dyn Clock,
        on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        self.scan_cf_range_pages_at(snapshot, cf, &KeyRange::all(), limit, clock, on_page)
    }

    /// Streams visible rows in bounded pages without reopening SST readers per page.
    pub fn scan_cf_range_pages_at<F, E>(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        limit: usize,
        clock: &dyn Clock,
        mut on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<(), E>,
        E: From<calyx_core::CalyxError>,
    {
        self.ensure_snapshot_live(snapshot, clock)
            .map_err(E::from)?;
        if limit == 0 {
            return Ok(());
        }
        if limit > LATEST_CF_RANGE_PAGE_MAX_ROWS {
            return Err(E::from(calyx_core::CalyxError {
                code: "CALYX_ASTER_SNAPSHOT_PAGE_INVALID",
                message: format!(
                    "snapshot page limit {limit} exceeds the hard maximum {LATEST_CF_RANGE_PAGE_MAX_ROWS}"
                ),
                remediation: "request a positive snapshot page size at or below LATEST_CF_RANGE_PAGE_MAX_ROWS",
            }));
        }
        if self.router_latest_readback.load(Ordering::Acquire) {
            self.ensure_router_latest_snapshot(snapshot)
                .map_err(E::from)?;
            let router = self.router.as_ref().ok_or_else(|| {
                E::from(calyx_core::CalyxError::aster_corrupt_shard(
                    "router-backed snapshot paging requested without a CF router".to_owned(),
                ))
            })?;

            // Hold rows until the router shard is acquired. A commit takes
            // those locks in the same order (rows -> router), so this hand-off
            // captures one exact physical snapshot: the immutable level at the
            // hand-off plus every changed row's state at `snapshot.seq()`.
            // The router's mutable tables are intentionally excluded below.
            let table = self.read_rows(RowGuardSite::SnapshotPagedOverlay, cf);
            let mut overlay = Vec::<crate::sst::SstEntry>::new();
            let mut overlay_bytes = 0_usize;
            if let Some(cf_rows) = table.get(&cf) {
                for (key, versions) in super::read::latest::overlay_range(cf_rows, cf, Some(range))?
                {
                    let value = versions
                        .iter()
                        .rev()
                        .find(|version| version.seq <= snapshot.seq())
                        .map_or(TOMBSTONE_VALUE, |version| version.value.as_slice());
                    let row_bytes =
                        crate::sst::materialized_entry_bytes::<crate::sst::SstEntry>(key, value);
                    overlay_bytes = overlay_bytes.saturating_add(row_bytes);
                    if overlay_bytes > SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES {
                        return Err(E::from(calyx_core::CalyxError {
                            code: "CALYX_ASTER_SNAPSHOT_OVERLAY_MEMORY_BUDGET",
                            message: format!(
                                "the {} snapshot delta needs {overlay_bytes} bytes, above the {}-byte transient overlay budget",
                                cf.name(),
                                SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES
                            ),
                            remediation: "checkpoint and compact the changed-key journal before retrying; the scan refuses unbounded overlay duplication instead of risking host memory exhaustion",
                        }));
                    }
                    overlay
                        .try_reserve(1)
                        .map_err(crate::sst::scan_reserve_failed)
                        .map_err(E::from)?;
                    overlay.push(crate::sst::SstEntry {
                        key: crate::sst::clone_scan_bytes(key).map_err(E::from)?,
                        value: crate::sst::clone_scan_bytes(value).map_err(E::from)?,
                    });
                }
            }

            router.range_immutable_pages_until(
                cf,
                range,
                limit,
                overlay,
                move || drop(table),
                |entries| {
                    self.ensure_snapshot_live(snapshot, clock)
                        .map_err(E::from)?;
                    let barriers = self
                        .read_barriers
                        .read()
                        .expect("mvcc read barriers poisoned");
                    for entry in &entries {
                        if let Some(error) = first_blocking(&barriers, cf, &entry.key) {
                            return Err(E::from(error));
                        }
                    }
                    drop(barriers);
                    on_page(
                        entries
                            .into_iter()
                            .map(|entry| (entry.key, entry.value))
                            .collect(),
                    )
                },
            )?;
            // A physically empty range has no callback in which to re-check
            // the lease. Validate once more after cursor exhaustion so an
            // operation that expired while opening its files never reports a
            // successful empty snapshot.
            self.ensure_snapshot_live(snapshot, clock)
                .map_err(E::from)?;
            return Ok(());
        }
        let mut after_key = None::<Vec<u8>>;
        loop {
            let page = self
                .scan_cf_range_page_at(snapshot, cf, range, after_key.as_deref(), limit, clock)
                .map_err(E::from)?;
            let Some(last_key) = page.last().map(|(key, _)| key.clone()) else {
                break;
            };
            after_key = Some(last_key);
            on_page(page)?;
        }
        Ok(())
    }
}
