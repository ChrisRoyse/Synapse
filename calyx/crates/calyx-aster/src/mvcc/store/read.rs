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
        self.with_latest_view(
            RowGuardSite::ReadLatest,
            cf,
            |seq, table, router, barriers| {
                ensure_view_key_unbarriered(barriers, cf, key)?;
                latest_value_from_view(seq, table.get(&cf), router, cf, key)
            },
        )
    }

    /// Resolves all requested CF/key rows from one atomic latest view.
    pub fn read_batch_latest(&self, reads: &[CfRead]) -> Result<Vec<Option<Vec<u8>>>> {
        if reads.is_empty() {
            return Ok(Vec::new());
        }
        self.with_latest_view_all(
            RowGuardSite::ReadBatchLatest,
            |seq, table, router, barriers| {
                reads
                    .iter()
                    .map(|read| {
                        ensure_view_key_unbarriered(barriers, read.cf, &read.key)?;
                        latest_value_from_view(seq, table.cf(read.cf), router, read.cf, &read.key)
                    })
                    .collect()
            },
        )
    }

    /// Scans one CF from one atomic view of the latest committed state.
    pub fn scan_cf_latest(&self, cf: ColumnFamily) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.with_latest_view(
            RowGuardSite::ScanCfLatest,
            cf,
            |seq, table, router, barriers| {
                latest_rows_from_view(seq, table, router, cf, None, barriers)
            },
        )
    }

    /// Counts the visible rows of one CF from one atomic view of the latest
    /// committed state, without materialising the rows (#1952).
    ///
    /// Equal by construction to `scan_cf_latest(cf)?.len()` — same view, same
    /// merge, same tombstone handling — and that equality is the contract these
    /// callers depend on, because they use the count as physical evidence that
    /// a write landed.
    pub fn count_cf_latest(&self, cf: ColumnFamily) -> Result<usize> {
        self.with_latest_view(
            RowGuardSite::CountCfLatest,
            cf,
            |seq, table, router, barriers| {
                latest_row_count_from_view(seq, table, router, cf, barriers)
            },
        )
    }

    /// Scans one CF range from one atomic view of the latest committed state.
    pub fn scan_cf_range_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.with_latest_view(
            RowGuardSite::ScanCfRangeLatest,
            cf,
            |seq, table, router, barriers| {
                latest_rows_from_view(seq, table, router, cf, Some(range), barriers)
            },
        )
    }

    /// Reads one candidate-bounded page from an atomic latest committed view.
    ///
    /// The exclusive `after_key` cursor and `resume_after` result include
    /// tombstoned keys. This guarantees forward progress even when a physical
    /// range contains no currently live rows. `limit` must not exceed
    /// [`LATEST_CF_RANGE_PAGE_MAX_ROWS`].
    pub fn scan_cf_range_page_latest(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<LatestCfRangePage> {
        validate_latest_page_request(range, after_key, limit)?;
        if limit == 0 {
            return Ok(LatestCfRangePage {
                snapshot_seq: self.current_seq(),
                rows: Vec::new(),
                resume_after: None,
                more: false,
                examined_rows: 0,
            });
        }
        self.with_latest_view(
            RowGuardSite::ScanCfRangePageLatest,
            cf,
            |seq, table, router, barriers| {
                latest_range_page_from_view(
                    seq,
                    table,
                    router,
                    barriers,
                    LatestRangePageRequest {
                        cf,
                        range,
                        after_key,
                        limit,
                    },
                )
            },
        )
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
            let table = self.read_rows(RowGuardSite::ReadAt, cf);
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
        let table = self.read_rows(RowGuardSite::SeqForKeyAt, cf);
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
            let table = self.read_rows_all(RowGuardSite::ReadBatch);
            for (index, read) in reads.iter().enumerate() {
                let visible = table
                    .cf(read.cf)
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

        let router = self.router.as_deref();
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
        self.overlay_table_rows(RowGuardSite::ScanCfAtOverlay, snapshot, cf, None, &mut rows)?;
        for key in rows.keys() {
            self.ensure_unbarriered(cf, key)?;
        }
        Ok(rows.into_iter().collect())
    }

    /// Returns keys with at least one committed version in
    /// `(after_exclusive, snapshot.seq()]`, including keys whose latest
    /// visible value is a tombstone.
    ///
    /// This is the exact MVCC delta surface used to reconcile an immutable
    /// persisted search generation with a newer pinned snapshot (#1842). A
    /// latest-only recovered router does not retain original per-row sequence
    /// numbers below `changed_key_history_floor`, so such a
    /// request fails closed and names the required rebase boundary.
    pub fn changed_keys_after_at(
        &self,
        snapshot: Snapshot,
        cf: ColumnFamily,
        after_exclusive: Seq,
        clock: &dyn Clock,
    ) -> Result<Vec<Vec<u8>>> {
        self.ensure_snapshot_live(snapshot, clock)?;
        if after_exclusive > snapshot.seq() {
            return Err(CalyxError::stale_derived(format!(
                "changed-key lower bound {after_exclusive} exceeds pinned snapshot seq {} for {}; refusing an inverted MVCC delta",
                snapshot.seq(),
                cf.name()
            )));
        }
        if after_exclusive < self.changed_key_history_floor {
            return Err(CalyxError::stale_derived(format!(
                "changed-key history for {} begins at recovered seq {}, but the requested delta starts after seq {after_exclusive}; rebuild the persisted search generation at or beyond the recovery floor before querying",
                cf.name(),
                self.changed_key_history_floor
            )));
        }
        let table = self.read_rows(RowGuardSite::ChangedKeysAfterAt, cf);
        let keys = table
            .get(&cf)
            .into_iter()
            .flat_map(|rows| rows.iter())
            .filter(|(_, versions)| {
                versions
                    .iter()
                    .any(|version| version.seq > after_exclusive && version.seq <= snapshot.seq())
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        drop(table);
        for key in &keys {
            self.ensure_unbarriered(cf, key)?;
        }
        Ok(keys)
    }

    /// The same MVCC changed-key delta as [`Self::changed_keys_after_at`], but
    /// over `Base` only and attributed to one exact panel version (#1901).
    ///
    /// `ColumnFamily::Base` holds the constellations of *every* panel, while a
    /// persisted search generation is panel-scoped. Counting the whole `Base`
    /// delta therefore charged one panel's reconciliation budget for every other
    /// panel's ingest: on the live vault a 329-row timeline generation measured
    /// 17,785 changed keys after 739 sequences, all of it agent-transcript
    /// churn, and every timeline query failed closed minutes after a full
    /// rebuild. Slot CFs need no equivalent scope because a slot id is allocated
    /// globally and belongs to exactly one panel.
    ///
    /// Attribution reads the row's own value bytes, never a separate disk read:
    /// a key that changed in the range has a version in this overlay table, and
    /// `decode_header` recovers `panel_version` from it. The whole visible chain
    /// is examined, not only its newest version, because a delete or a
    /// cross-panel move must still be charged to the panel that indexed the row.
    ///
    /// A key whose entire visible history is tombstoned — the live version it
    /// replaced has already been compacted out of the overlay — cannot be
    /// attributed. Those keys are returned *with* the panel's keys and counted
    /// separately: the changed set is used only to mask rows out of an immutable
    /// generation, so masking a key that generation never held is a no-op, while
    /// dropping a key it did hold would serve a deleted row.
    pub fn changed_base_keys_after_at_for_panel(
        &self,
        snapshot: Snapshot,
        after_exclusive: Seq,
        panel_version: u32,
        clock: &dyn Clock,
    ) -> Result<PanelScopedChangedKeys> {
        let keys =
            self.changed_keys_after_at(snapshot, ColumnFamily::Base, after_exclusive, clock)?;
        let scanned = keys.len();
        let table = self.read_rows(
            RowGuardSite::ChangedBaseKeysAfterAtForPanel,
            ColumnFamily::Base,
        );
        let mut scoped = Vec::new();
        let mut other_panels = 0_usize;
        let mut unattributed = 0_usize;
        for key in keys {
            match visible_base_panels_in_chain(&table, &key, snapshot.seq())? {
                ChainPanels::Panels(panels) if panels.contains(&panel_version) => {
                    scoped.push(key);
                }
                ChainPanels::Panels(_) => other_panels += 1,
                ChainPanels::Unattributable => {
                    unattributed += 1;
                    scoped.push(key);
                }
            }
        }
        drop(table);
        Ok(PanelScopedChangedKeys {
            panel_version,
            scanned,
            panel: scoped.len() - unattributed,
            other_panels,
            unattributed,
            keys: scoped,
        })
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
        self.overlay_table_rows(
            RowGuardSite::ScanCfRangeAtOverlay,
            snapshot,
            cf,
            Some(range),
            &mut rows,
        )?;
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
        self.overlay_table_keys(snapshot, cf, range, &mut keys)?;
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
                        // Deliberately does NOT say the key "disappeared". Both
                        // halves of this read run at one pinned sequence, so
                        // nothing can vanish between them; the only way to be
                        // here is that the key-selection view and the
                        // value-resolution view disagree about visibility at
                        // that same sequence. Naming a race sent an
                        // investigation at a deterministic condition after the
                        // wrong thing (#1954), so the message states the
                        // disagreement and names both views instead.
                        calyx_core::CalyxError::aster_corrupt_shard(format!(
                            "{} key {} was selected as visible at pinned seq {} by the \
                             key view (router range keys + MVCC table overlay) but the \
                             value view (read_batch) resolved no live value at that same \
                             sequence; the two latest views disagree about this key's \
                             visibility, which is deterministic at a pinned sequence and \
                             not a concurrent mutation",
                            cf.name(),
                            hex_prefix(&key),
                            snapshot.seq()
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
        let table = self.read_rows(RowGuardSite::ScanCfRangePageAt, cf);
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
        let table = self.read_rows(RowGuardSite::PredecessorCfAt, cf);
        let router = self.router.as_deref();
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

    /// `site` is the **caller's** name, not this helper's.
    ///
    /// Five entry points share this body — `read_latest`, `read_batch_latest`,
    /// `scan_cf_latest`, `scan_cf_range_latest`, `scan_cf_range_page_latest` —
    /// and they differ enormously: two are point reads and one is an unbounded
    /// full-CF scan. Reporting `with_latest_view` for all of them named the
    /// helper and left the actual holder still unidentified, which is the exact
    /// failure #1950 ask 1 exists to end (the first live run of this instrument
    /// reported 189 of 191 holds as `with_latest_view`, which narrowed the
    /// search by one level and then stopped).
    fn with_latest_view<T>(
        &self,
        site: RowGuardSite,
        cf: ColumnFamily,
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
        let table = self.read_rows(site, cf);
        let router = self.router.as_deref();
        let seq = self.current_seq();
        read(seq, &table, router, &barriers)
    }

    /// [`Self::with_latest_view`] for the one entry point whose reads may name
    /// any set of column families. Takes every shard, in index order.
    fn with_latest_view_all<T>(
        &self,
        site: RowGuardSite,
        read: impl FnOnce(Seq, &TimedRowReadAll<'_>, Option<&CfRouter>, &[ReadBarrier]) -> Result<T>,
    ) -> Result<T> {
        let barriers = self
            .read_barriers
            .read()
            .expect("mvcc read barriers poisoned");
        let table = self.read_rows_all(site);
        let router = self.router.as_deref();
        let seq = self.current_seq();
        read(seq, &table, router, &barriers)
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
    cf_rows: Option<&BTreeMap<Vec<u8>, VersionChain>>,
    router: Option<&CfRouter>,
    cf: ColumnFamily,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    if let Some(state) = cf_rows
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

/// Counts the visible rows of one CF from an atomic latest view, without
/// materialising a single value into the result.
///
/// Line-for-line the same merge as [`latest_rows_from_view`] — same router
/// source, same `is_tombstone_value` filter, same table overlay with
/// `Live` inserting and `Tombstone` removing, same barrier check over the
/// surviving keys. The only difference is that it accumulates a
/// `BTreeSet<Vec<u8>>` of keys instead of a `BTreeMap<Vec<u8>, Vec<u8>>` and
/// returns the length.
///
/// That difference is the point (#1952). Fourteen call sites were written as
/// `scan_cf_latest(cf)?.len()`, which builds a full key/value map and then a
/// `Vec<(Vec<u8>, Vec<u8>)>` of every row in the CF purely to read `.len()`.
/// The values are the expensive part — these are panels of dense slot vectors —
/// and every one of them was cloned twice and dropped. On the live vault `Base`
/// holds ~96,845 rows.
///
/// It does **not** avoid reading values: tombstone status is only knowable from
/// the value, so `router.iter_cf` still produces them. The honest description of
/// the win is "two fewer copies of every value", not "fewer bytes read".
///
/// A keys-only router path would be needed for the latter. #1954 asserted such a
/// path would also be *wrong*, because `range_keys_until` supposedly could not
/// see a flushed tombstone. **That is false and was corrected by measurement**:
/// `SstLevel::range_keys_until` carries `is_tombstone` per key and filters
/// tombstoned keys out before returning, so the `rows.insert(key, false)` in
/// `CfRouter::range_keys_until` only ever receives keys already known live. The
/// original reading stopped at the router and did not follow into the level.
///
/// `examples/router_latest_tombstone_fsv.rs` proves this on a handle whose
/// `router_latest_readback` is read back as `true`, across all four merge
/// states (flushed tombstone, memtable-resident tombstone, resurrection,
/// tombstone for a never-written key): the key view and the value view agree,
/// and agree on the independently-known correct answer.
///
/// So the reason to keep counting from values is the honest one — it is the
/// same view `scan_cf_latest` uses, and these counts are evidence that a write
/// landed — not a tombstone defect in the keys path.
fn latest_row_count_from_view(
    seq: Seq,
    table: &RowTable,
    router: Option<&CfRouter>,
    cf: ColumnFamily,
    barriers: &[ReadBarrier],
) -> Result<usize> {
    let mut keys = router
        .map(|router| router.iter_cf(cf))
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| (!is_tombstone_value(&row.value)).then_some(row.key))
        .collect::<BTreeSet<_>>();

    if let Some(cf_rows) = table.get(&cf) {
        for (key, versions) in cf_rows {
            match visible_value_state(versions, seq) {
                Some(VisibleValue::Live(_)) => {
                    keys.insert(key.clone());
                }
                Some(VisibleValue::Tombstone) => {
                    keys.remove(key);
                }
                None => {}
            }
        }
    }
    for key in &keys {
        ensure_view_key_unbarriered(barriers, cf, key)?;
    }
    Ok(keys.len())
}

struct LatestRangePageRequest<'a> {
    cf: ColumnFamily,
    range: &'a KeyRange,
    after_key: Option<&'a [u8]>,
    limit: usize,
}

fn latest_range_page_from_view(
    seq: Seq,
    table: &RowTable,
    router: Option<&CfRouter>,
    barriers: &[ReadBarrier],
    request: LatestRangePageRequest<'_>,
) -> Result<LatestCfRangePage> {
    let LatestRangePageRequest {
        cf,
        range,
        after_key,
        limit,
    } = request;
    let candidate_limit = limit + 1;
    let router_rows = router
        .map(|router| {
            router.range_candidate_page_until(
                cf,
                &range.start,
                range.end.as_deref(),
                after_key,
                candidate_limit,
            )
        })
        .transpose()?
        .unwrap_or_default();
    let table_rows =
        table_candidate_page_from_view(table, seq, cf, range, after_key, candidate_limit)?;

    let mut router_index = 0;
    let mut table_index = 0;
    let mut candidates = Vec::with_capacity(candidate_limit);
    while candidates.len() < candidate_limit
        && (router_index < router_rows.len() || table_index < table_rows.len())
    {
        let (key, state) = match (router_rows.get(router_index), table_rows.get(table_index)) {
            (Some(router), Some((table_key, table_state))) => match router.key.cmp(table_key) {
                std::cmp::Ordering::Less => {
                    router_index += 1;
                    (
                        router.key.clone(),
                        visible_state_from_router_value(&router.value),
                    )
                }
                std::cmp::Ordering::Greater => {
                    table_index += 1;
                    (table_key.clone(), table_state.clone())
                }
                std::cmp::Ordering::Equal => {
                    router_index += 1;
                    table_index += 1;
                    (table_key.clone(), table_state.clone())
                }
            },
            (Some(router), None) => {
                router_index += 1;
                (
                    router.key.clone(),
                    visible_state_from_router_value(&router.value),
                )
            }
            (None, Some((table_key, table_state))) => {
                table_index += 1;
                (table_key.clone(), table_state.clone())
            }
            (None, None) => break,
        };
        candidates.push((key, state));
    }

    let examined_rows = candidates.len();
    let more = examined_rows > limit;
    // The lookahead candidate is part of the read. Validate its barrier before
    // revealing `more=true`; otherwise paging would leak the existence of a
    // blocked live row even though the row itself is not emitted yet.
    for (key, state) in &candidates {
        if matches!(state, VisibleValue::Live(_)) {
            ensure_view_key_unbarriered(barriers, cf, key)?;
        }
    }
    let mut resume_after = None;
    let mut rows = Vec::with_capacity(limit);
    for (key, state) in candidates.into_iter().take(limit) {
        resume_after = Some(key.clone());
        if let VisibleValue::Live(value) = state {
            rows.push((key, value));
        }
    }

    Ok(LatestCfRangePage {
        snapshot_seq: seq,
        rows,
        resume_after,
        more,
        examined_rows,
    })
}

fn table_candidate_page_from_view(
    table: &RowTable,
    seq: Seq,
    cf: ColumnFamily,
    range: &KeyRange,
    after_key: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, VisibleValue)>> {
    let Some(cf_rows) = table.get(&cf) else {
        return Ok(Vec::new());
    };
    let lower = after_key
        .map(|key| Bound::Excluded(key.to_vec()))
        .unwrap_or_else(|| Bound::Included(range.start.clone()));
    let upper = range
        .end
        .as_ref()
        .map(|key| Bound::Excluded(key.clone()))
        .unwrap_or(Bound::Unbounded);
    cf_rows
        .range((lower, upper))
        .map(|(key, versions)| {
            visible_value_state(versions, seq)
                .map(|state| (key.clone(), state))
                .ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "latest {} row {} has no version visible at sequence {seq}",
                        cf.name(),
                        hex_prefix(key)
                    ))
                })
        })
        .take(limit)
        .collect()
}

fn visible_state_from_router_value(value: &[u8]) -> VisibleValue {
    if is_tombstone_value(value) {
        VisibleValue::Tombstone
    } else {
        VisibleValue::Live(value.to_vec())
    }
}

fn validate_latest_page_request(
    range: &KeyRange,
    after_key: Option<&[u8]>,
    limit: usize,
) -> Result<()> {
    if limit > LATEST_CF_RANGE_PAGE_MAX_ROWS {
        return Err(invalid_latest_page(format!(
            "latest range page limit {limit} exceeds the hard maximum {LATEST_CF_RANGE_PAGE_MAX_ROWS}"
        )));
    }
    if range
        .end
        .as_ref()
        .is_some_and(|end| end.as_slice() <= range.start.as_slice())
    {
        return Err(invalid_latest_page(format!(
            "latest range page requires end > start; start={} end={}",
            hex_prefix(&range.start),
            range
                .end
                .as_deref()
                .map(hex_prefix)
                .unwrap_or_else(|| "<unbounded>".to_owned())
        )));
    }
    if let Some(after_key) = after_key
        && (after_key < range.start.as_slice()
            || range
                .end
                .as_ref()
                .is_some_and(|end| after_key >= end.as_slice()))
    {
        return Err(invalid_latest_page(format!(
            "latest range page cursor {} is outside [{}, {})",
            hex_prefix(after_key),
            hex_prefix(&range.start),
            range
                .end
                .as_deref()
                .map(hex_prefix)
                .unwrap_or_else(|| "+inf".to_owned())
        )));
    }
    Ok(())
}

fn invalid_latest_page(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: "CALYX_ASTER_RANGE_PAGE_INVALID",
        message: message.into(),
        remediation: "supply an ordered range, an exclusive cursor inside that range, and a page limit at or below LATEST_CF_RANGE_PAGE_MAX_ROWS",
    }
}

fn visible_value(versions: &[VersionedValue], seq: Seq) -> Option<Vec<u8>> {
    visible_value_state(versions, seq).and_then(VisibleValue::into_option)
}

#[derive(Clone)]
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
