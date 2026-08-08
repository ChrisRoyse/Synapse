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
        // Refuse up front, naming the declaration (#1973).
        //
        // Without this the request proceeds to the SST layer and fails with
        // `CALYX_ASTER_SST_PAGE_INDEX_MISSING` naming a *file path*, which
        // describes a symptom: the caller learns some SST lacks an index, not
        // that this whole family was never declared pageable. That refusal also
        // only fires once a family has SSTs on disk, so paging a
        // not-yet-flushed family appears to work and starts failing later —
        // which is precisely how a bounded readback over `XTerm` reached
        // production before anyone noticed.
        if !cf.supports_paged_scan() {
            return Err(CalyxError {
                code: "CALYX_ASTER_CF_NOT_PAGEABLE",
                message: format!(
                    "candidate-bounded paging was requested for {}, which is not declared pageable; only families for which a validated SST lookup index is retained at open can be paged",
                    cf.name()
                ),
                remediation: "add this family to ColumnFamily::supports_paged_scan (which the open-time lookup-retention policy reads), or use an unpaged scan; do not fall back to a whole-file scan",
            });
        }
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

    /// Counts the visible rows of one CF in the **MVCC row table alone**, with
    /// the CF router excluded whether or not this vault would consult it.
    ///
    /// This exists to make the full-restore invariant *checkable* rather than
    /// assumed. [`Self::latest_router_source`] gates the router off whenever
    /// `router_latest_readback` is `false`, on the stated ground that a
    /// `restore_mvcc_rows: true` open materialises the whole corpus into the row
    /// table. That ground is load-bearing — if it were false, gating would drop
    /// rows — and the only honest way to hold it is to compare this count
    /// against the same family read through a second handle opened
    /// `restore_mvcc_rows: false`, whose view is the router alone. Equal counts
    /// and equal key/value digests are what proves the two sources agree.
    ///
    /// Deliberately *not* gated on `router_latest_readback`: on a latest-only
    /// handle this reports the row table's own (near-empty) contents, which is
    /// exactly what makes a harness that opened the wrong mode fail loudly
    /// instead of reporting a vacuous match (#1954).
    ///
    /// **Diagnostic only.** It is `O(family)` under a single hold of the
    /// row-table read guard — measured at 28.5 ms over a 112,559-row `Base`,
    /// against a 25 ms budget — which is the very shape #1977 exists to remove
    /// from serving paths. It is counted under its own
    /// [`RowGuardSite::CountCfLatestTableOnly`] so such a hold can never be
    /// mistaken in the census for one a reader actually served from. Do not
    /// wire it into health, a maintenance tick, or any per-request path; use
    /// `count_cf_latest_bounded` for those.
    pub fn latest_row_count_table_only(&self, cf: ColumnFamily) -> usize {
        let seq = self.current_seq();
        let table = self.read_rows(RowGuardSite::CountCfLatestTableOnly, cf);
        table.get(&cf).map_or(0, |cf_rows| {
            cf_rows
                .values()
                .filter(|versions| {
                    matches!(
                        visible_value_state(versions, seq),
                        Some(VisibleValue::Live(_))
                    )
                })
                .count()
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
        self.overlay_table_rows(
            RowGuardSite::ScanCfAtOverlay,
            snapshot,
            cf,
            None,
            &mut rows,
            clock,
        )?;
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
    ///
    /// # Cost
    ///
    /// `O(1)` when nothing changed, `O(family)` when something did (#2139). The
    /// name promises a delta and the fold is a scan: there is no per-sequence
    /// index to seek into, so producing the key *list* still costs a walk of
    /// the family. What the per-family commit sequence removes is the far more
    /// common question — *is the list empty?* — which used to cost the same
    /// walk. Callers that only want to know whether anything changed should ask
    /// [`Self::latest_seq_for_cf`] directly and never enter this function.
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
        // `O(1)` exit for the answer this is asked for most often: nothing
        // (#2139).
        //
        // Paging fixed how long the guard is held; it did not change that the
        // fold visits every key of the family and every key's whole version
        // chain, so "did anything change?" cost the same whether the answer was
        // 0 or 1,000,000 — and the search-generation freshness trigger asks it
        // on every decision, against a ~1 M-row `Base`.
        //
        // The per-family commit sequence answers it exactly. Every version in
        // this family was written by a commit that published
        // `last_commit_seq >= its own seq` before the version became visible, so
        // `last_commit_seq <= after_exclusive` means no version anywhere in the
        // family satisfies `seq > after_exclusive`, and the delta is empty.
        // There are no keys to return and therefore none to barrier-check,
        // which is the same result the full fold produces on this input.
        //
        // This is a proof, not a heuristic, and it fails toward the walk: if the
        // signal has moved at all — including from a commit to another family
        // that shares a slot-signal cell — the full fold below runs.
        if self.cf_change_signal(cf).last_commit_seq <= after_exclusive {
            return Ok(Vec::new());
        }
        // Folded in bounded pages, releasing the row read guard between them
        // (#2060). The single hold this replaces was `O(family)` — 117 ms over
        // a 1,039,273-row `Base` on the deployed daemon, 17 of its 20 lifetime
        // holds over the 25 ms budget — and every commit in the process waits
        // on that guard.
        //
        // Paging is exact here, not an approximation, for three reasons that
        // hold together:
        //
        // 1. **The cursor cannot skip a key.** Row-table entries are only ever
        //    created (`entry(key).or_default()`); nothing removes a key, not
        //    even snapshot GC, which trims version chains in place. So the key
        //    order this cursor walks is append-only and stable.
        // 2. **A concurrent commit cannot enter the answer.** It allocates a
        //    sequence above `snapshot.seq()`, and the predicate admits only
        //    versions at or below it. Keys created after the pin therefore fold
        //    to "unchanged" wherever the cursor happens to meet them.
        // 3. **A concurrent reclaim cannot remove the answer.** GC keeps each
        //    chain's newest version at or below the safe point, and the safe
        //    point is clamped to the oldest live lease — this snapshot's
        //    included. A version it does drop is strictly older than the one it
        //    keeps, so if any dropped version satisfied `seq > after_exclusive`
        //    the retained boundary version satisfies it too. No key can leave
        //    the delta by being compacted.
        //
        // The lease is re-checked on every page rather than once at entry: it
        // is what makes 2 and 3 true, so a fold that outlived it must fail
        // closed instead of reading a view whose versions have started being
        // reclaimed.
        let mut keys = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            self.ensure_snapshot_live(snapshot, clock)?;
            let mut page_end: Option<Vec<u8>> = None;
            {
                let table = self.read_rows(RowGuardSite::ChangedKeysAfterAt, cf);
                let Some(cf_rows) = table.get(&cf) else {
                    break;
                };
                let lower = cursor.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                let mut examined = 0_usize;
                for (key, versions) in cf_rows.range::<[u8], _>((lower, Bound::Unbounded)) {
                    if versions.iter().any(|version| {
                        version.seq > after_exclusive && version.seq <= snapshot.seq()
                    }) {
                        keys.push(key.clone());
                    }
                    examined += 1;
                    if examined == ROW_GUARD_FOLD_PAGE_ROWS {
                        page_end = Some(key.clone());
                        break;
                    }
                }
            }
            // A page that ended before the limit reached the end of the family.
            let Some(page_end) = page_end else {
                break;
            };
            cursor = Some(page_end);
        }
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
        let mut scoped = Vec::new();
        let mut other_panels = 0_usize;
        let mut unattributed = 0_usize;
        // Attribution runs in bounded chunks under short guard holds (#2060).
        // It is the more expensive of the two halves — every changed key's
        // whole visible chain is decoded through `decode_header` — and on the
        // deployed daemon it held the guard for 539 ms, over budget on 100% of
        // its lifetime invocations, stalling every commit for half a second.
        //
        // Attribution is per key and order-independent, so splitting it across
        // holds cannot change any key's outcome. What a chunk boundary does
        // admit is a snapshot-GC reclaim landing between chunks and trimming a
        // superseded version out of a not-yet-attributed chain. That is the
        // condition this function already names and already answers
        // conservatively: a chain with no attributable visible version is
        // `Unattributable` and is returned *with* the panel's keys, because
        // masking a key the generation never held is a no-op while dropping one
        // it did hold would serve a deleted row. Reclaim is not excluded by the
        // unpaged shape either — it runs on its own tick, before the fold as
        // readily as between its chunks — so the guarantee here is unchanged,
        // and the lease is re-checked per chunk so an expired pin fails closed.
        for chunk in keys.chunks(ROW_GUARD_FOLD_PAGE_ROWS) {
            self.ensure_snapshot_live(snapshot, clock)?;
            let table = self.read_rows(
                RowGuardSite::ChangedBaseKeysAfterAtForPanel,
                ColumnFamily::Base,
            );
            for key in chunk {
                match visible_base_panels_in_chain(&table, key, snapshot.seq())? {
                    ChainPanels::Panels(panels) if panels.contains(&panel_version) => {
                        scoped.push(key.clone());
                    }
                    ChainPanels::Panels(_) => other_panels += 1,
                    ChainPanels::Unattributable => {
                        unattributed += 1;
                        scoped.push(key.clone());
                    }
                }
            }
        }
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
            clock,
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
        let router = self.latest_router_source();
        let seq = self.current_seq();
        read(seq, &table, router, &barriers)
    }

    /// The physical source a **latest**-view read may consult beside the MVCC
    /// row table, or `None` when the row table is the whole authority.
    ///
    /// `router_latest_readback` is `false` exactly when the vault was opened
    /// with `restore_mvcc_rows: true`, which materialises every manifested SST
    /// batch plus the WAL tail into the row table (`durable.rs`
    /// `read_manifested_batches` + `replay_records_into`). The row table then
    /// holds the complete corpus, and every *snapshot* reader already says so
    /// by construction: [`Self::router_latest_value`],
    /// [`Self::router_latest_rows`] and [`Self::router_latest_keys`] all return
    /// empty in that mode, and [`Self::predecessor_cf_at`] skips the router
    /// candidate outright.
    ///
    /// The six latest-view entry points did not, and that gap is #1978. They
    /// read the router unconditionally, so on the live daemon — which opens
    /// `full_mvcc_restore` — every bounded page of a `Base` walk re-opened the
    /// family's SSTs and re-read a page of values off disk **while holding the
    /// vault-wide row-table read guard every constellation writer needs**
    /// (#1950), only to discard the result: the merge gives the row table
    /// precedence on every key both sources hold, and in this mode there is no
    /// key only the router holds. That measured 32,982 holds and 151 s of guard
    /// occupancy from one site in 28 minutes, 95% of every over-budget hold.
    ///
    /// This is a gate, not a fallback: when the flag is `true` the router is
    /// still required and still consulted. Nothing here degrades to a
    /// best-effort answer, and no caller gets a partial view — the two modes
    /// have disjoint, individually complete sources.
    fn latest_router_source(&self) -> Option<&CfRouter> {
        if self.router_latest_readback.load(Ordering::Acquire) {
            self.router.as_deref()
        } else {
            None
        }
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
        let router = self.latest_router_source();
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
        // Seek the range; do not walk the family and filter (#2036).
        //
        // This is the same defect #1973 fixed in the overlay, left behind in the
        // latest-view reader: it iterated every key in the CF's row BTreeMap and
        // applied `KeyRange::contains` as a filter, so a prefix read of a dozen
        // anchors cost the whole family — while holding the vault-wide row-table
        // read guard every constellation writer needs (#1950).
        //
        // Measured on the live daemon before this change: 4,417
        // CALYX_ASTER_ROW_READ_GUARD_SLOW warnings, 3,772 of them (85%) from
        // `scan_cf_range_latest`, with 512 of 512 sampled commits over the
        // duration floor and /health taking 13.5-23.7 s on every request because
        // every writer queued behind these reads.
        //
        // `overlay_range` is shared with the overlay reader rather than
        // reimplemented, so both paths keep one definition of the bounds and one
        // inverted-range contract. The bounds do not change which keys are
        // visited: a BTreeMap range over `[start, end)` yields exactly the keys
        // for which `KeyRange::contains` is true, because `contains` is
        // start-inclusive and end-exclusive.
        for (key, versions) in latest::overlay_range(cf_rows, cf, range)? {
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
/// Manual FSV proves this on a handle whose `router_latest_readback` is read
/// back as `true`, across all four merge
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
