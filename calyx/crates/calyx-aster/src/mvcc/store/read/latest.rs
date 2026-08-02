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
        let router = self.router.read().expect("mvcc router poisoned");
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
        let router = self.router.read().expect("mvcc router poisoned");
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
        let router = self.router.read().expect("mvcc router poisoned");
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
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: Option<&KeyRange>,
        rows: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    ) {
        let table = self.read_rows(RowGuardSite::OverlayTableRows);
        let Some(cf_rows) = table.get(&cf) else {
            return;
        };
        for (key, versions) in cf_rows {
            if range.is_some_and(|range| !range.contains(key)) {
                continue;
            }
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
    }

    pub(super) fn overlay_table_keys(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        keys: &mut BTreeMap<Vec<u8>, ()>,
    ) {
        let table = self.read_rows(RowGuardSite::OverlayTableKeys);
        let Some(cf_rows) = table.get(&cf) else {
            return;
        };
        for (key, versions) in cf_rows {
            if !range.contains(key) {
                continue;
            }
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
