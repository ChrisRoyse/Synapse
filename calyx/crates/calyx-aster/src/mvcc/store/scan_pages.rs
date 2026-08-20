use super::*;

/// Re-check a long-lived snapshot lease at this row cadence. The underlying
/// row cursor holds no corpus-wide value page, so this is a time/liveness bound
/// only, not a memory batching knob.
const SNAPSHOT_ROW_STREAM_LEASE_CHECK_ROWS: usize = 256;

/// One pinned, allocation-reusing CF row stream.
pub(crate) struct SnapshotCfRowStream<'a> {
    store: &'a VersionedCfStore,
    snapshot: Snapshot,
    cf: ColumnFamily,
    clock: &'a dyn Clock,
    rows_until_lease_check: usize,
    renew_lease_while_progressing: bool,
    lease_renewal_count: u64,
    inner: SnapshotCfRowStreamInner<'a>,
}

enum SnapshotCfRowStreamInner<'a> {
    Router(crate::cf::CfImmutableRowStream<'a>),
    Paged {
        range: KeyRange,
        after_key: Option<Vec<u8>>,
        page: std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
        exhausted: bool,
    },
}

impl SnapshotCfRowStream<'_> {
    /// Keeps this stream's exact pinned snapshot live while the consumer is
    /// actively advancing it. Renewal preserves the reader id and sequence;
    /// an expired/missing lease fails closed and is never replaced by a newer
    /// snapshot.
    pub(crate) fn renew_lease_while_progressing(mut self) -> Self {
        self.renew_lease_while_progressing = true;
        self
    }

    pub(crate) const fn lease_renewal_count(&self) -> u64 {
        self.lease_renewal_count
    }

    fn refresh_or_validate_lease(&mut self) -> Result<()> {
        if self.renew_lease_while_progressing {
            self.snapshot = self.store.renew_snapshot(self.snapshot, self.clock)?;
            self.lease_renewal_count = self.lease_renewal_count.checked_add(1).ok_or_else(|| {
                CalyxError {
                    code: "CALYX_ASTER_READER_LEASE_RENEWAL_OVERFLOW",
                    message: format!(
                        "reader lease {} renewal count exceeds the durable u64 limit",
                        self.snapshot.lease().id()
                    ),
                    remediation: "stop the scan and inspect its progress accounting; do not continue with an uncounted lease renewal",
                }
            })?;
            Ok(())
        } else {
            self.store.ensure_snapshot_live(self.snapshot, self.clock)
        }
    }

    /// Lends the next visible row to `visit`; returns `false` at EOF.
    pub(crate) fn next_with(
        &mut self,
        visit: impl FnOnce(&[u8], &[u8]) -> Result<()>,
    ) -> Result<bool> {
        if self.rows_until_lease_check == 0 {
            self.refresh_or_validate_lease()?;
            self.rows_until_lease_check = SNAPSHOT_ROW_STREAM_LEASE_CHECK_ROWS;
        }

        let store = self.store;
        let snapshot = self.snapshot;
        let cf = self.cf;
        let clock = self.clock;
        let mut visit = Some(visit);
        let present = match &mut self.inner {
            SnapshotCfRowStreamInner::Router(stream) => stream.next_with(|key, value| {
                let barriers = store
                    .read_barriers
                    .read()
                    .expect("mvcc read barriers poisoned");
                if let Some(error) = first_blocking(&barriers, cf, key) {
                    return Err(error);
                }
                drop(barriers);
                visit
                    .take()
                    .expect("snapshot row visitor is consumed exactly once")(
                    key, value
                )
            })?,
            SnapshotCfRowStreamInner::Paged {
                range,
                after_key,
                page,
                exhausted,
            } => loop {
                if let Some((key, value)) = page.next() {
                    visit
                        .take()
                        .expect("snapshot row visitor is consumed exactly once")(
                        &key, &value
                    )?;
                    break true;
                }
                if *exhausted {
                    break false;
                }
                let rows = store.scan_cf_range_page_at(
                    snapshot,
                    cf,
                    range,
                    after_key.as_deref(),
                    1,
                    clock,
                )?;
                let Some(last_key) = rows.last().map(|(key, _)| key.clone()) else {
                    *exhausted = true;
                    continue;
                };
                *after_key = Some(last_key);
                *page = rows.into_iter();
            },
        };
        if present {
            self.rows_until_lease_check -= 1;
        } else {
            self.refresh_or_validate_lease()?;
        }
        Ok(present)
    }
}

impl VersionedCfStore {
    /// Opens one coherent row-at-a-time scan over a pinned snapshot.
    ///
    /// Router-backed stores retain one reusable value buffer per intersecting
    /// SST source plus a bounded MVCC overlay; they never build an output page
    /// or a corpus-wide row vector. The in-memory fallback also advances one
    /// row at a time and exists for non-router vaults only.
    pub(crate) fn open_cf_range_row_stream_at<'a>(
        &'a self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        clock: &'a dyn Clock,
    ) -> Result<SnapshotCfRowStream<'a>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        let inner = if self.router_latest_readback.load(Ordering::Acquire) {
            self.ensure_router_latest_snapshot(snapshot)?;
            let router = self.router.as_ref().ok_or_else(|| {
                CalyxError::aster_corrupt_shard(
                    "router-backed snapshot row stream requested without a CF router".to_owned(),
                )
            })?;

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
                        return Err(CalyxError {
                            code: "CALYX_ASTER_SNAPSHOT_OVERLAY_MEMORY_BUDGET",
                            message: format!(
                                "the {} snapshot row-stream delta needs {overlay_bytes} bytes, above the {}-byte transient overlay budget",
                                cf.name(),
                                SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES
                            ),
                            remediation: "checkpoint and compact the changed-key journal before retrying; the row stream refuses unbounded overlay duplication instead of risking host memory exhaustion",
                        });
                    }
                    overlay
                        .try_reserve(1)
                        .map_err(crate::sst::scan_reserve_failed)?;
                    overlay.push(crate::sst::SstEntry {
                        key: crate::sst::clone_scan_bytes(key)?,
                        value: crate::sst::clone_scan_bytes(value)?,
                    });
                }
            }
            SnapshotCfRowStreamInner::Router(router.open_immutable_row_stream(
                cf,
                range,
                overlay,
                move || drop(table),
            )?)
        } else {
            SnapshotCfRowStreamInner::Paged {
                range: range.clone(),
                after_key: None,
                page: Vec::new().into_iter(),
                exhausted: false,
            }
        };
        Ok(SnapshotCfRowStream {
            store: self,
            snapshot,
            cf,
            clock,
            rows_until_lease_check: 0,
            renew_lease_while_progressing: false,
            lease_renewal_count: 0,
            inner,
        })
    }

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

    /// Counts visible rows at one pinned sequence without materializing
    /// immutable values.
    ///
    /// Router-backed stores stream every physical key state exactly once,
    /// validate each record's index key and CRC, merge newest-wins, and count
    /// only non-tombstone winners. The bounded MVCC overlay and rows -> router
    /// lock hand-off are the same snapshot boundary used by value paging.
    pub fn count_cf_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        limit: usize,
        clock: &dyn Clock,
    ) -> Result<(usize, usize)> {
        self.ensure_snapshot_live(snapshot, clock)?;
        if limit == 0 || limit > LATEST_CF_RANGE_PAGE_MAX_ROWS {
            return Err(calyx_core::CalyxError {
                code: "CALYX_ASTER_SNAPSHOT_COUNT_PAGE_INVALID",
                message: format!(
                    "snapshot count page limit {limit} is outside 1..={LATEST_CF_RANGE_PAGE_MAX_ROWS}"
                ),
                remediation: "request a positive count page size at or below LATEST_CF_RANGE_PAGE_MAX_ROWS",
            });
        }

        if self.router_latest_readback.load(Ordering::Acquire) {
            self.ensure_router_latest_snapshot(snapshot)?;
            let router = self.router.as_ref().ok_or_else(|| {
                calyx_core::CalyxError::aster_corrupt_shard(
                    "router-backed snapshot count requested without a CF router".to_owned(),
                )
            })?;
            let range = KeyRange::all();

            // Capture the exact changed-key journal while rows are pinned,
            // then transfer that pin atomically to the immutable router level.
            let table = self.read_rows(RowGuardSite::SnapshotPagedOverlay, cf);
            let mut overlay = Vec::<crate::sst::SstEntry>::new();
            let mut overlay_bytes = 0_usize;
            if let Some(cf_rows) = table.get(&cf) {
                for (key, versions) in
                    super::read::latest::overlay_range(cf_rows, cf, Some(&range))?
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
                        return Err(calyx_core::CalyxError {
                            code: "CALYX_ASTER_SNAPSHOT_OVERLAY_MEMORY_BUDGET",
                            message: format!(
                                "the {} snapshot count delta needs {overlay_bytes} bytes, above the {}-byte transient overlay budget",
                                cf.name(),
                                SNAPSHOT_ROUTER_OVERLAY_MAX_BYTES
                            ),
                            remediation: "checkpoint and compact the changed-key journal before retrying; exact counting refuses unbounded overlay duplication instead of risking host memory exhaustion",
                        });
                    }
                    overlay
                        .try_reserve(1)
                        .map_err(crate::sst::scan_reserve_failed)?;
                    overlay.push(crate::sst::SstEntry {
                        key: crate::sst::clone_scan_bytes(key)?,
                        value: crate::sst::clone_scan_bytes(value)?,
                    });
                }
            }

            let mut rows = 0_usize;
            let mut pages = 0_usize;
            router.range_immutable_key_state_pages_until(
                cf,
                &range,
                limit,
                overlay,
                move || drop(table),
                |states| {
                    self.ensure_snapshot_live(snapshot, clock)?;
                    let barriers = self
                        .read_barriers
                        .read()
                        .expect("mvcc read barriers poisoned");
                    for state in states {
                        if state.is_tombstone {
                            continue;
                        }
                        if let Some(error) = first_blocking(&barriers, cf, &state.key) {
                            return Err(error);
                        }
                        rows = rows.checked_add(1).ok_or_else(|| calyx_core::CalyxError {
                            code: "CALYX_ASTER_SNAPSHOT_COUNT_OVERFLOW",
                            message: format!(
                                "the exact {} row count exceeded usize on this host",
                                cf.name()
                            ),
                            remediation: "inspect the immutable manifest and repair the impossible row cardinality before retrying",
                        })?;
                    }
                    pages = pages.checked_add(1).ok_or_else(|| calyx_core::CalyxError {
                        code: "CALYX_ASTER_SNAPSHOT_COUNT_OVERFLOW",
                        message: format!(
                            "the exact {} count page total exceeded usize on this host",
                            cf.name()
                        ),
                        remediation: "inspect the immutable manifest and repair the impossible page cardinality before retrying",
                    })?;
                    Ok(())
                },
            )?;
            self.ensure_snapshot_live(snapshot, clock)?;
            return Ok((rows, pages.max(1)));
        }

        let mut rows = 0_usize;
        let mut pages = 0_usize;
        self.scan_cf_pages_at(
            snapshot,
            cf,
            limit,
            clock,
            |page| -> Result<()> {
                rows = rows.checked_add(page.len()).ok_or_else(|| {
                    calyx_core::CalyxError {
                        code: "CALYX_ASTER_SNAPSHOT_COUNT_OVERFLOW",
                        message: format!(
                            "the exact {} row count exceeded usize on this host",
                            cf.name()
                        ),
                        remediation: "inspect the resident MVCC table and repair the impossible row cardinality before retrying",
                    }
                })?;
                pages = pages.checked_add(1).ok_or_else(|| calyx_core::CalyxError {
                    code: "CALYX_ASTER_SNAPSHOT_COUNT_OVERFLOW",
                    message: format!(
                        "the exact {} count page total exceeded usize on this host",
                        cf.name()
                    ),
                    remediation: "inspect the resident MVCC table and repair the impossible page cardinality before retrying",
                })?;
                Ok(())
            },
        )?;
        Ok((rows, pages.max(1)))
    }
}
