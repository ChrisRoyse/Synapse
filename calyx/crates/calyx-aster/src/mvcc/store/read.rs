use super::*;

mod latest;

impl VersionedCfStore {
    /// Reads one CF/key from one atomic view of the latest committed state.
    ///
    /// Unlike composing [`Self::current_seq`] with [`Self::read_at`], this
    /// method holds the same row/router read-side synchronization boundary
    /// that excludes a concurrent commit while the latest sequence and both
    /// serving layers are resolved. That distinction is required for
    /// latest-only recovered routers, which intentionally cannot serve a
    /// sequence that becomes historical between two separate calls.
    pub fn read_latest(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.with_latest_view(|seq, table, router, barriers| {
            ensure_view_key_unbarriered(barriers, cf, key)?;
            latest_value_from_view(seq, table, router, cf, key)
        })
    }

    /// Resolves all requested CF/key rows from one atomic latest view.
    pub fn read_batch_latest(&self, reads: &[CfRead]) -> Result<Vec<Option<Vec<u8>>>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        self.with_latest_view(|seq, table, router, barriers| {
            reads
                .iter()
                .map(|read| {
                    ensure_view_key_unbarriered(barriers, read.cf, &read.key)?;
                    latest_value_from_view(seq, table, router, read.cf, &read.key)
                })
                .collect()
        })
    }

    /// Scans one CF from one atomic view of the latest committed state.
    pub fn scan_cf_latest(&self, cf: ColumnFamily) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.with_latest_view(|seq, table, router, barriers| {
            latest_rows_from_view(seq, table, router, cf, None, barriers)
        })
    }

    /// Scans one CF range from one atomic view of the latest committed state.
    pub fn scan_cf_range_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.with_latest_view(|seq, table, router, barriers| {
            latest_rows_from_view(seq, table, router, cf, Some(range), barriers)
        })
    }

    /// Reads one CF/key at the pinned sequence.
    pub fn read_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        key: &[u8],
        clock: &dyn Clock,
    ) -> Result<Option<Vec<u8>>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        self.ensure_unbarriered(cf, key)?;
        {
            let table = self.rows.read().expect("mvcc row table poisoned");
            if let Some(value) = table
                .get(&cf)
                .and_then(|rows| rows.get(key))
                .and_then(|versions| visible_value_state(versions, snapshot.seq()))
            {
                return Ok(value.into_option());
            }
        }
        self.router_latest_value(snapshot, cf, key)
    }

    /// Returns the visible version sequence for one CF/key at the pinned sequence.
    pub fn seq_for_key_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        key: &[u8],
        clock: &dyn Clock,
    ) -> Result<Option<Seq>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        self.ensure_unbarriered(cf, key)?;
        let table = self.rows.read().expect("mvcc row table poisoned");
        let seq = table
            .get(&cf)
            .and_then(|rows| rows.get(key))
            .and_then(|versions| visible_version(versions, snapshot.seq()))
            .map(|version| version.seq);
        if seq.is_some() || !self.router_latest_readback.load(Ordering::Acquire) {
            return Ok(seq);
        }
        self.ensure_router_latest_snapshot(snapshot)?;
        Err(latest_only_error(format!(
            "row sequence for {} key {} is unavailable because this vault was opened in latest-only recovery mode",
            cf.name(),
            hex_prefix(key)
        )))
    }

    /// Resolves all requested CF/key rows at the same pinned sequence.
    pub fn read_batch(
        &self,
        snapshot: Snapshot,
        reads: &[CfRead],
        clock: &dyn Clock,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        if reads.is_empty() {
            return Ok(Vec::new());
        }

        // Hold one shared barrier generation across table and router
        // resolution. An installer needs the write guard, so no requested key
        // can become blocked halfway through this logical batch.
        let barriers = self
            .read_barriers
            .read()
            .expect("mvcc read barriers poisoned");
        for read in reads {
            if let Some(error) = first_blocking(&barriers, read.cf, &read.key) {
                return Err(error);
            }
        }

        let mut values = vec![None; reads.len()];
        let mut router_misses = Vec::new();
        {
            let table = self.rows.read().expect("mvcc row table poisoned");
            for (index, read) in reads.iter().enumerate() {
                let visible = table
                    .get(&read.cf)
                    .and_then(|rows| rows.get(read.key.as_slice()))
                    .and_then(|versions| visible_value_state(versions, snapshot.seq()));
                match visible {
                    Some(VisibleValue::Live(value)) => values[index] = Some(value),
                    Some(VisibleValue::Tombstone) => {}
                    None => router_misses.push(index),
                }
            }
        }

        if router_misses.is_empty() || !self.router_latest_readback.load(Ordering::Acquire) {
            return Ok(values);
        }

        let router = self.router.read().expect("mvcc router poisoned");
        self.ensure_router_latest_snapshot(snapshot)?;
        if let Some(router) = router.as_ref() {
            for index in router_misses {
                let read = &reads[index];
                values[index] = router
                    .get(read.cf, &read.key)?
                    .filter(|value| !is_tombstone_value(value));
            }
        }
        Ok(values)
    }

    /// Scans visible rows for one CF at the pinned sequence, ordered by key.
    pub fn scan_cf_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        clock: &dyn Clock,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        let mut rows = self.router_latest_rows(snapshot, cf, None)?;
        self.overlay_table_rows(snapshot, cf, None, &mut rows);
        for key in rows.keys() {
            self.ensure_unbarriered(cf, key)?;
        }
        Ok(rows.into_iter().collect())
    }

    /// Scans visible rows for one CF and key range at the pinned sequence.
    pub fn scan_cf_range_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        clock: &dyn Clock,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        let mut rows = self.router_latest_rows(snapshot, cf, Some(range))?;
        self.overlay_table_rows(snapshot, cf, Some(range), &mut rows);
        for key in rows.keys() {
            self.ensure_unbarriered(cf, key)?;
        }
        Ok(rows.into_iter().collect())
    }

    /// Scans visible row keys for one CF and key range at the pinned sequence.
    pub fn scan_cf_range_keys_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        clock: &dyn Clock,
    ) -> Result<Vec<Vec<u8>>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        let mut keys = self.router_latest_keys(snapshot, cf, range)?;
        self.overlay_table_keys(snapshot, cf, range, &mut keys);
        for key in keys.keys() {
            self.ensure_unbarriered(cf, key)?;
        }
        Ok(keys.into_keys().collect())
    }

    /// Scans at most `limit` visible rows in a range after `after_key`.
    pub fn scan_cf_range_page_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
        clock: &dyn Clock,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        if self.router_latest_readback.load(Ordering::Acquire) {
            // Select visible keys first so table tombstones and inserts are
            // merged correctly without cloning the complete value range.
            let keys = self.scan_cf_range_keys_at(snapshot, cf, range, clock)?;
            let keys = keys
                .into_iter()
                .filter(|key| after_key.is_none_or(|after| key.as_slice() > after))
                .take(limit)
                .collect::<Vec<_>>();
            let reads = keys
                .iter()
                .cloned()
                .map(|key| CfRead::new(cf, key))
                .collect::<Vec<_>>();
            let values = self.read_batch(snapshot, &reads, clock)?;
            return keys
                .into_iter()
                .zip(values)
                .map(|(key, value)| {
                    value.map(|value| (key.clone(), value)).ok_or_else(|| {
                        calyx_core::CalyxError::aster_corrupt_shard(format!(
                            "visible {} key {} disappeared during pinned page read",
                            cf.name(),
                            hex_prefix(&key)
                        ))
                    })
                })
                .collect();
        }
        let lower = if let Some(after_key) = after_key {
            Bound::Excluded(after_key)
        } else {
            Bound::Included(range.start.as_slice())
        };
        let table = self.rows.read().expect("mvcc row table poisoned");
        let mut rows = Vec::with_capacity(limit);
        let Some(cf_rows) = table.get(&cf) else {
            return Ok(rows);
        };
        for (key, versions) in cf_rows.range::<[u8], _>((lower, Bound::Unbounded)) {
            if !range.contains(key) {
                if range.end.as_ref().is_some_and(|end| key >= end) {
                    break;
                }
                continue;
            }
            if let Some(value) = visible_value(versions, snapshot.seq()) {
                self.ensure_unbarriered(cf, key)?;
                rows.push((key.clone(), value));
                if rows.len() == limit {
                    break;
                }
            }
        }
        Ok(rows)
    }

    /// Returns the greatest visible row in `[start, upper]` at one pinned snapshot.
    pub fn predecessor_cf_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        start: &[u8],
        upper: &[u8],
        clock: &dyn Clock,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        let barriers = self
            .read_barriers
            .read()
            .expect("mvcc read barriers poisoned");
        let table = self.rows.read().expect("mvcc row table poisoned");
        let router = self.router.read().expect("mvcc router poisoned");
        if self.router_latest_readback.load(Ordering::Acquire) {
            self.ensure_router_latest_snapshot(snapshot)?;
        }
        let mut upper = upper.to_vec();
        let mut inclusive = true;
        loop {
            let table_candidate =
                table_predecessor_state_from_view(&table, snapshot, cf, start, &upper, inclusive);
            let router_candidate = if self.router_latest_readback.load(Ordering::Acquire) {
                router
                    .as_ref()
                    .map(|router| router.predecessor(cf, start, &upper, inclusive))
                    .transpose()?
                    .flatten()
            } else {
                None
            };
            match (table_candidate, router_candidate) {
                (None, None) => return Ok(None),
                (None, Some(row)) => {
                    ensure_view_key_unbarriered(&barriers, cf, &row.key)?;
                    return Ok(Some((row.key, row.value)));
                }
                (Some((key, VisibleValue::Live(value))), None) => {
                    ensure_view_key_unbarriered(&barriers, cf, &key)?;
                    return Ok(Some((key, value)));
                }
                (Some((key, VisibleValue::Tombstone)), None) => {
                    upper = key;
                    inclusive = false;
                }
                (Some((key, state)), Some(router_row)) => {
                    if router_row.key > key {
                        ensure_view_key_unbarriered(&barriers, cf, &router_row.key)?;
                        return Ok(Some((router_row.key, router_row.value)));
                    }
                    match state {
                        VisibleValue::Live(value) => {
                            ensure_view_key_unbarriered(&barriers, cf, &key)?;
                            return Ok(Some((key, value)));
                        }
                        VisibleValue::Tombstone => {
                            upper = key;
                            inclusive = false;
                        }
                    }
                }
            }
        }
    }

    pub(super) fn ensure_unbarriered(&self, cf: ColumnFamily, key: &[u8]) -> Result<()> {
        let barriers = self
            .read_barriers
            .read()
            .expect("mvcc read barriers poisoned");
        if let Some(error) = first_blocking(&barriers, cf, key) {
            return Err(error);
        }
        Ok(())
    }

    fn with_latest_view<T>(
        &self,
        read: impl FnOnce(Seq, &RowTable, Option<&CfRouter>, &[ReadBarrier]) -> Result<T>,
    ) -> Result<T> {
        // Lock order is deliberately identical to every multi-layer reader:
        // barriers -> rows -> router. Commits acquire rows -> router. Holding
        // the rows guard prevents sequence allocation, and holding the router
        // guard prevents a physical serving-view refresh, so `current_seq`
        // and both sources below describe one atomic latest view.
        let barriers = self
            .read_barriers
            .read()
            .expect("mvcc read barriers poisoned");
        let table = self.rows.read().expect("mvcc row table poisoned");
        let router = self.router.read().expect("mvcc router poisoned");
        let seq = self.current_seq();
        read(seq, &table, router.as_ref(), &barriers)
    }
}

fn table_predecessor_state_from_view(
    table: &RowTable,
    snapshot: Snapshot,
    cf: ColumnFamily,
    start: &[u8],
    upper: &[u8],
    inclusive: bool,
) -> Option<(Vec<u8>, VisibleValue)> {
    let lower = Bound::Included(start);
    let upper = if inclusive {
        Bound::Included(upper)
    } else {
        Bound::Excluded(upper)
    };
    table
        .get(&cf)?
        .range::<[u8], _>((lower, upper))
        .rev()
        .find_map(|(key, versions)| {
            visible_value_state(versions, snapshot.seq()).map(|state| (key.clone(), state))
        })
}

fn ensure_view_key_unbarriered(
    barriers: &[ReadBarrier],
    cf: ColumnFamily,
    key: &[u8],
) -> Result<()> {
    if let Some(error) = first_blocking(barriers, cf, key) {
        return Err(error);
    }
    Ok(())
}

fn latest_value_from_view(
    seq: Seq,
    table: &RowTable,
    router: Option<&CfRouter>,
    cf: ColumnFamily,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    if let Some(state) = table
        .get(&cf)
        .and_then(|rows| rows.get(key))
        .and_then(|versions| visible_value_state(versions, seq))
    {
        return Ok(state.into_option());
    }
    router
        .map(|router| router.get(cf, key))
        .transpose()
        .map(|value| value.flatten().filter(|value| !is_tombstone_value(value)))
}

fn latest_rows_from_view(
    seq: Seq,
    table: &RowTable,
    router: Option<&CfRouter>,
    cf: ColumnFamily,
    range: Option<&KeyRange>,
    barriers: &[ReadBarrier],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut rows = match (router, range) {
        (Some(router), Some(range)) => match range.end.as_deref() {
            Some(end) => router.range(cf, &range.start, end)?,
            None => router
                .iter_cf(cf)?
                .into_iter()
                .filter(|row| row.key.as_slice() >= range.start.as_slice())
                .collect(),
        },
        (Some(router), None) => router.iter_cf(cf)?,
        (None, _) => Vec::new(),
    }
    .into_iter()
    .filter_map(|row| (!is_tombstone_value(&row.value)).then_some((row.key, row.value)))
    .collect::<BTreeMap<_, _>>();

    if let Some(cf_rows) = table.get(&cf) {
        for (key, versions) in cf_rows {
            if range.is_some_and(|range| !range.contains(key)) {
                continue;
            }
            match visible_value_state(versions, seq) {
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
    for key in rows.keys() {
        ensure_view_key_unbarriered(barriers, cf, key)?;
    }
    Ok(rows.into_iter().collect())
}

fn visible_value(versions: &[VersionedValue], seq: Seq) -> Option<Vec<u8>> {
    visible_value_state(versions, seq).and_then(VisibleValue::into_option)
}

enum VisibleValue {
    Live(Vec<u8>),
    Tombstone,
}

impl VisibleValue {
    fn into_option(self) -> Option<Vec<u8>> {
        match self {
            Self::Live(value) => Some(value),
            Self::Tombstone => None,
        }
    }
}

fn visible_value_state(versions: &[VersionedValue], seq: Seq) -> Option<VisibleValue> {
    visible_version(versions, seq).map(|version| {
        if is_tombstone_value(&version.value) {
            VisibleValue::Tombstone
        } else {
            VisibleValue::Live(version.value.clone())
        }
    })
}

fn visible_version(versions: &[VersionedValue], seq: Seq) -> Option<&VersionedValue> {
    versions.iter().rev().find(|version| version.seq <= seq)
}

fn latest_only_error(message: impl Into<String>) -> calyx_core::CalyxError {
    calyx_core::CalyxError {
        code: "CALYX_ASTER_LATEST_ONLY_HISTORY_UNAVAILABLE",
        message: message.into(),
        remediation: "open the vault with full MVCC recovery before requesting historical row state",
    }
}

pub(super) fn hex_prefix(bytes: &[u8]) -> String {
    let mut value = String::new();
    for byte in bytes.iter().take(12) {
        value.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > 12 {
        value.push_str("...");
    }
    value
}
