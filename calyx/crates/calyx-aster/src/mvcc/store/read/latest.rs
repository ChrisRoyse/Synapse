use super::*;

impl VersionedCfStore {
    pub(super) fn router_latest_value(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        if !self.router_latest_readback.load(Ordering::Acquire) {
            return Ok(None);
        }
        let router = self.router.as_ref();
        // Keep the router read guard across the latest-sequence check and
        // lookup. Commits take the router write guard before publishing their
        // sequence, so a snapshot cannot pass validation and then observe a
        // newer physical serving view.
        self.ensure_router_latest_snapshot(snapshot)?;
        let Some(router) = router.as_ref() else {
            return Ok(None);
        };
        Ok(router
            .get(cf, key)?
            .filter(|value| !is_tombstone_value(value)))
    }

    pub(super) fn router_latest_rows(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: Option<&KeyRange>,
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        if !self.router_latest_readback.load(Ordering::Acquire) {
            return Ok(BTreeMap::new());
        }
        let router = self.router.as_ref();
        self.ensure_router_latest_snapshot(snapshot)?;
        let Some(router) = router.as_ref() else {
            return Ok(BTreeMap::new());
        };
        let rows = match range {
            Some(range) => match range.end.as_deref() {
                Some(end) => router.range(cf, &range.start, end)?,
                None => router
                    .iter_cf(cf)?
                    .into_iter()
                    .filter(|row| row.key.as_slice() >= range.start.as_slice())
                    .collect(),
            },
            None => router.iter_cf(cf)?,
        };
        Ok(rows
            .into_iter()
            .filter_map(|row| (!is_tombstone_value(&row.value)).then_some((row.key, row.value)))
            .collect())
    }

    pub(super) fn router_latest_keys(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<BTreeMap<Vec<u8>, ()>> {
        if !self.router_latest_readback.load(Ordering::Acquire) {
            return Ok(BTreeMap::new());
        }
        let router = self.router.as_ref();
        self.ensure_router_latest_snapshot(snapshot)?;
        let Some(router) = router.as_ref() else {
            return Ok(BTreeMap::new());
        };
        Ok(router
            .range_keys_until(cf, &range.start, range.end.as_deref())?
            .into_iter()
            .map(|key| (key, ()))
            .collect())
    }

    /// Merges the MVCC overlay's visible rows over the router's rows, in
    /// bounded pages that release the row read guard between them (#2060).
    ///
    /// The single hold this replaces was the largest guard hold ever observed
    /// on the deployed daemon: **757 ms** at `scan_cf_at_overlay`, three
    /// quarters of a second during which every commit in the process was
    /// blocked waiting for the write guard it needs exclusively. #1973 had
    /// already narrowed the *rows visited* by seeking the range instead of
    /// filtering the family; a whole-family `scan_cf_at` has no range to seek,
    /// so the only remaining lever is the length of the critical section.
    ///
    /// **Paging does not change the merged result.** Each key's contribution —
    /// insert its visible value, or remove a key the overlay tombstones — is
    /// independent of every other key's and is applied exactly once, because
    /// row-table entries are only ever created, never removed, so the key order
    /// this cursor walks is stable. A commit landing between pages allocates a
    /// sequence above `snapshot.seq()`, so `visible_value_state` reports it as
    /// not-yet-visible wherever the cursor meets it; a reclaim landing between
    /// pages keeps each chain's newest version at or below the safe point,
    /// which is clamped to the oldest live lease, so the version visible at
    /// this pinned sequence is never the one dropped. The lease is re-checked
    /// per page so a fold that outlived its pin fails closed.
    pub(super) fn overlay_table_rows(
        &self,
        site: RowGuardSite,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: Option<&KeyRange>,
        rows: &mut BTreeMap<Vec<u8>, Vec<u8>>,
        clock: &dyn Clock,
    ) -> Result<()> {
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            self.ensure_snapshot_live(snapshot, clock)?;
            let mut page_end: Option<Vec<u8>> = None;
            {
                let table = self.read_rows(site, cf);
                let Some(cf_rows) = table.get(&cf) else {
                    return Ok(());
                };
                let mut examined = 0_usize;
                for (key, versions) in overlay_page(cf_rows, cf, range, cursor.as_deref())? {
                    match visible_value_state(versions, snapshot.seq()) {
                        Some(VisibleValue::Live(value)) => {
                            rows.insert(key.clone(), value);
                        }
                        Some(VisibleValue::Tombstone) => {
                            rows.remove(key);
                        }
                        None => {}
                    }
                    examined += 1;
                    if examined == ROW_GUARD_FOLD_PAGE_ROWS {
                        page_end = Some(key.clone());
                        break;
                    }
                }
            }
            // A page that ended before the limit exhausted the requested range.
            let Some(page_end) = page_end else {
                return Ok(());
            };
            cursor = Some(page_end);
        }
    }

    pub(super) fn overlay_table_keys(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        keys: &mut BTreeMap<Vec<u8>, ()>,
    ) -> Result<()> {
        let table = self.read_rows(RowGuardSite::OverlayTableKeys, cf);
        let Some(cf_rows) = table.get(&cf) else {
            return Ok(());
        };
        for (key, versions) in overlay_range(cf_rows, cf, Some(range))? {
            match visible_value_state(versions, snapshot.seq()) {
                Some(VisibleValue::Live(_)) => {
                    keys.insert(key.clone(), ());
                }
                Some(VisibleValue::Tombstone) => {
                    keys.remove(key);
                }
                None => {}
            }
        }
        Ok(())
    }

    pub(in crate::mvcc::store) fn ensure_router_latest_snapshot(
        &self,
        snapshot: Snapshot,
    ) -> Result<()> {
        let latest = self.current_seq();
        let history_floor = self.changed_key_history_floor.load(Ordering::Acquire);
        if snapshot.seq() >= history_floor && snapshot.seq() <= latest {
            return Ok(());
        }
        if snapshot.seq() > latest {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "snapshot {} is ahead of the latest committed sequence {latest}",
                snapshot.seq()
            )));
        }
        Err(CalyxError {
            code: "CALYX_ASTER_ROUTER_HISTORY_BEFORE_RECOVERY_FLOOR",
            message: format!(
                "snapshot {} predates the disk-backed MVCC recovery floor {}; latest committed sequence is {latest}",
                snapshot.seq(),
                history_floor
            ),
            remediation: "rebase the reader at or after the reported recovery floor; this process preserves every later change as an exact in-memory delta over the durable router baseline",
        })
    }

    pub(in crate::mvcc::store) fn ensure_snapshot_live(
        &self,
        snapshot: Snapshot,
        clock: &dyn Clock,
    ) -> Result<()> {
        let now = clock.now();
        let lease = snapshot.lease();
        if lease.is_expired_at(now) {
            self.leases.abort_if_expired(lease, now);
        }
        lease.ensure_live_at(now)
    }
}

/// The overlay rows a scan must examine, **seeking** to the requested range
/// instead of walking the whole column family and discarding what falls
/// outside it.
///
/// The row overlay is a `BTreeMap`, so the range is already sorted and the
/// bounds are a `O(log n)` descent plus one walk of the matched span. The
/// previous shape iterated every key in the family and applied
/// [`KeyRange::contains`] as a filter, which made a narrow-range read cost the
/// whole family — and it paid that cost *while holding the row-table read
/// guard* that every constellation writer needs (#1950). On the live vault a
/// prefix read of `Graph` (81,236 rows) held the guard for **452 ms** against a
/// 25 ms budget (#1973). The bounds do not change which keys are visited: a
/// `BTreeMap` range over `[start, end)` yields exactly the keys for which
/// `KeyRange::contains` is true.
///
/// # Errors
///
/// Fails closed when `end < start`. `BTreeMap::range` panics on an inverted
/// range, and the filter shape this replaces silently returned nothing — so an
/// inverted range read as "this family is empty" instead of "this request is
/// malformed". Neither is acceptable; an empty-but-ordered range (`end ==
/// start`) is legal and yields nothing, as it did before.
pub(in crate::mvcc::store) fn overlay_range<'a>(
    cf_rows: &'a BTreeMap<Vec<u8>, VersionChain>,
    cf: ColumnFamily,
    range: Option<&KeyRange>,
) -> Result<std::collections::btree_map::Range<'a, Vec<u8>, VersionChain>> {
    let Some(range) = range else {
        return Ok(cf_rows.range::<[u8], _>((Bound::Unbounded, Bound::Unbounded)));
    };
    let end = match range.end.as_deref() {
        Some(end) if end < range.start.as_slice() => {
            return Err(CalyxError {
                code: "CALYX_ASTER_OVERLAY_RANGE_INVERTED",
                message: format!(
                    "overlay range read of {} requires end >= start; start={} end={}",
                    cf.name(),
                    super::hex_prefix(&range.start),
                    super::hex_prefix(end)
                ),
                remediation: "supply an ordered KeyRange; an inverted range is a caller bug, not an empty column family",
            });
        }
        Some(end) => Bound::Excluded(end),
        None => Bound::Unbounded,
    };
    Ok(cf_rows.range::<[u8], _>((Bound::Included(range.start.as_slice()), end)))
}

/// [`overlay_range`] resumed after an exclusive cursor, for folds that release
/// the row read guard between pages (#2060).
///
/// The upper bound and the inversion check are `overlay_range`'s, unchanged —
/// the cursor only raises the lower bound, and it raises it to a key the
/// previous page already visited, so the two together yield each key in the
/// requested range exactly once and in order.
///
/// # Errors
///
/// Fails closed on an inverted range, exactly as [`overlay_range`] does.
pub(super) fn overlay_page<'a>(
    cf_rows: &'a BTreeMap<Vec<u8>, VersionChain>,
    cf: ColumnFamily,
    range: Option<&KeyRange>,
    after: Option<&[u8]>,
) -> Result<std::collections::btree_map::Range<'a, Vec<u8>, VersionChain>> {
    let Some(after) = after else {
        return overlay_range(cf_rows, cf, range);
    };
    let end = match range {
        Some(range) => match range.end.as_deref() {
            Some(end) if end < range.start.as_slice() => {
                return Err(CalyxError {
                    code: "CALYX_ASTER_OVERLAY_RANGE_INVERTED",
                    message: format!(
                        "overlay range read of {} requires end >= start; start={} end={}",
                        cf.name(),
                        super::hex_prefix(&range.start),
                        super::hex_prefix(end)
                    ),
                    remediation: "supply an ordered KeyRange; an inverted range is a caller bug, not an empty column family",
                });
            }
            Some(end) => Bound::Excluded(end),
            None => Bound::Unbounded,
        },
        None => Bound::Unbounded,
    };
    Ok(cf_rows.range::<[u8], _>((Bound::Excluded(after), end)))
}
