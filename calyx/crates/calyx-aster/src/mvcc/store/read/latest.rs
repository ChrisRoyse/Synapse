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

    pub(super) fn overlay_table_rows(
        &self,
        site: RowGuardSite,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: Option<&KeyRange>,
        rows: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> Result<()> {
        let table = self.read_rows(site, cf);
        let Some(cf_rows) = table.get(&cf) else {
            return Ok(());
        };
        for (key, versions) in overlay_range(cf_rows, cf, range)? {
            match visible_value_state(versions, snapshot.seq()) {
                Some(VisibleValue::Live(value)) => {
                    rows.insert(key.clone(), value);
                }
                Some(VisibleValue::Tombstone) => {
                    rows.remove(key);
                }
                None => {}
            }
        }
        Ok(())
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
        if snapshot.seq() == latest {
            return Ok(());
        }
        Err(latest_only_error(format!(
            "historical snapshot {} requested from latest-only recovered vault at seq {}",
            snapshot.seq(),
            latest
        )))
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
fn overlay_range<'a>(
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
