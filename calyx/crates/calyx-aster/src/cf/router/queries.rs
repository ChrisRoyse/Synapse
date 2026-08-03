use super::*;

impl CfRouter {
    pub fn get(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let shard = self.read_shard(cf)?;
        // Newest source first, first hit wins: active memtable, then each
        // sealed memtable awaiting its SST, then the SST levels (#1949).
        for table in shard.read_tables(cf) {
            if let Some(value) = table.get(key) {
                return Ok(Some(value));
            }
        }
        shard.levels.get(&cf).map_or(Ok(None), |level| {
            level
                .get(key)?
                .map(|value| self.config.open_value(cf, key, value))
                .transpose()
        })
    }

    pub fn range(&self, cf: ColumnFamily, start: &[u8], end: &[u8]) -> Result<Vec<SstEntry>> {
        let shard = self.read_shard(cf)?;
        let mut rows = BTreeMap::new();
        if let Some(level) = shard.levels.get(&cf) {
            for entry in level.range(start, end)? {
                rows.insert(entry.key, entry.value);
            }
        }
        // Oldest in-memory source first so a newer one overwrites it.
        for table in shard.read_tables_oldest_first(cf) {
            for (key, value) in table.range(start, end) {
                rows.insert(key, value);
            }
        }
        self.open_entries(
            cf,
            rows.into_iter().map(|(key, value)| SstEntry { key, value }),
        )
    }

    /// Returns the greatest live row at or below `upper` without materializing the range.
    pub(crate) fn predecessor(
        &self,
        cf: ColumnFamily,
        start: &[u8],
        upper: &[u8],
        inclusive: bool,
    ) -> Result<Option<SstEntry>> {
        let shard = self.read_shard(cf)?;
        let mut upper = upper.to_vec();
        let mut inclusive = inclusive;
        loop {
            let level = shard
                .levels
                .get(&cf)
                .map(|level| level.predecessor(start, &upper, inclusive))
                .transpose()?
                .flatten();
            // The greatest key across every in-memory source; on a tie the
            // newest source wins, so scan newest first and keep strict `>`.
            let mut memtable: Option<SstEntry> = None;
            for table in shard.read_tables(cf) {
                if let Some((key, value)) = table.predecessor(start, &upper, inclusive)
                    && memtable.as_ref().is_none_or(|current| key > current.key)
                {
                    memtable = Some(SstEntry { key, value });
                }
            }
            let candidate = match (level, memtable) {
                (None, None) => return Ok(None),
                (Some(entry), None) | (None, Some(entry)) => entry,
                (Some(level), Some(memtable)) => {
                    if memtable.key >= level.key {
                        memtable
                    } else {
                        level
                    }
                }
            };
            let SstEntry { key, value } = candidate;
            let value = self.config.open_value(cf, &key, value)?;
            let candidate = SstEntry { key, value };
            if !crate::mvcc::is_tombstone_value(&candidate.value) {
                return Ok(Some(candidate));
            }
            upper = candidate.key;
            inclusive = false;
        }
    }

    pub fn range_page_until(
        &self,
        cf: ColumnFamily,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<SstEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let shard = self.read_shard(cf)?;
        let mut merged = BTreeMap::new();
        for table in shard.read_tables_oldest_first(cf) {
            for (key, value) in table.range_until(start, end) {
                merged.insert(key, value);
            }
        }
        let overlay = merged
            .into_iter()
            .map(|(key, value)| SstEntry { key, value })
            .collect::<Vec<_>>();
        let rows = shard
            .levels
            .get(&cf)
            .cloned()
            .unwrap_or_default()
            .range_page_with_overlay(start, end, after_key, limit, overlay)?;
        self.config.open_entries(cf, rows)
    }

    /// Returns at most `limit` newest raw key states after `after_key`.
    ///
    /// The page retains tombstones so the MVCC row overlay can merge one
    /// bounded ordered candidate stream without losing deletion precedence.
    /// Both immutable and mutable sources contribute at most `limit` rows;
    /// the two-way merge emits only the first `limit` keys in their union.
    pub(crate) fn range_candidate_page_until(
        &self,
        cf: ColumnFamily,
        start: &[u8],
        end: Option<&[u8]>,
        after_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<SstEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let shard = self.read_shard(cf)?;
        let immutable = shard
            .levels
            .get(&cf)
            .map(|level| level.range_candidate_page_until(start, end, after_key, limit))
            .transpose()?
            .unwrap_or_default();
        // Mutable router rows are plaintext until flush; immutable SST rows
        // may be sealed. Open only the immutable source before merging so an
        // encrypted vault never tries to decrypt a plaintext memtable winner.
        let immutable = self.config.open_entries(cf, immutable)?;
        // Each in-memory source contributes at most `limit` rows; merging them
        // oldest-first into an ordered map keeps newest-wins precedence, and
        // taking `limit` afterwards preserves the bounded-page contract.
        let mut merged = BTreeMap::new();
        for table in shard.read_tables_oldest_first(cf) {
            for (key, value) in table.range_candidate_page_until(start, end, after_key, limit) {
                merged.insert(key, value);
            }
        }
        let mutable = merged
            .into_iter()
            .take(limit)
            .map(|(key, value)| SstEntry { key, value })
            .collect::<Vec<_>>();

        let mut immutable_index = 0;
        let mut mutable_index = 0;
        let mut rows = Vec::with_capacity(limit);
        while rows.len() < limit
            && (immutable_index < immutable.len() || mutable_index < mutable.len())
        {
            let next = match (immutable.get(immutable_index), mutable.get(mutable_index)) {
                (Some(immutable), Some(mutable)) => match immutable.key.cmp(&mutable.key) {
                    std::cmp::Ordering::Less => {
                        immutable_index += 1;
                        immutable.clone()
                    }
                    std::cmp::Ordering::Greater => {
                        mutable_index += 1;
                        mutable.clone()
                    }
                    std::cmp::Ordering::Equal => {
                        immutable_index += 1;
                        mutable_index += 1;
                        mutable.clone()
                    }
                },
                (Some(immutable), None) => {
                    immutable_index += 1;
                    immutable.clone()
                }
                (None, Some(mutable)) => {
                    mutable_index += 1;
                    mutable.clone()
                }
                (None, None) => break,
            };
            rows.push(next);
        }
        Ok(rows)
    }

    pub fn range_keys(&self, cf: ColumnFamily, start: &[u8], end: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.range_keys_until(cf, start, Some(end))
    }

    pub fn range_keys_until(
        &self,
        cf: ColumnFamily,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Vec<Vec<u8>>> {
        let shard = self.read_shard(cf)?;
        let mut rows = BTreeMap::<Vec<u8>, bool>::new();
        if let Some(level) = shard.levels.get(&cf) {
            for key in level.range_keys_until(start, end)? {
                rows.insert(key, false);
            }
        }
        for table in shard.read_tables_oldest_first(cf) {
            for (key, value) in table.range_until(start, end) {
                rows.insert(key, crate::mvcc::is_tombstone_value(&value));
            }
        }
        Ok(rows
            .into_iter()
            .filter_map(|(key, is_tombstone)| (!is_tombstone).then_some(key))
            .collect())
    }

    pub fn iter_cf(&self, cf: ColumnFamily) -> Result<Vec<SstEntry>> {
        let shard = self.read_shard(cf)?;
        let mut rows = BTreeMap::new();
        if let Some(level) = shard.levels.get(&cf) {
            for entry in level.iter()? {
                rows.insert(entry.key, entry.value);
            }
        }
        for table in shard.read_tables_oldest_first(cf) {
            for (key, value) in table.iter() {
                rows.insert(key, value);
            }
        }
        self.config.open_entries(
            cf,
            rows.into_iter().map(|(key, value)| SstEntry { key, value }),
        )
    }

    /// # Panics
    ///
    /// Panics when the shard owning `cf` is poisoned.
    pub fn level_file_count(&self, cf: ColumnFamily) -> usize {
        self.read_shard(cf)
            .expect("CF router shard poisoned during level file count")
            .levels
            .get(&cf)
            .map_or(0, SstLevel::file_count)
    }

    /// Raw flush with no commit domain; see [`Self::flush_pending_at`].
    pub fn flush_pending(&self) -> Result<Vec<SstSummary>> {
        self.flush_pending_at(NO_COMMIT_DOMAIN)
    }

    /// Flushes every non-empty memtable at `commit_watermark`; see
    /// [`Self::flush_cf_at`] for the watermark contract.
    pub fn flush_pending_at(&self, commit_watermark: u64) -> Result<Vec<SstSummary>> {
        // The candidate list is collected under the shard guards and they are
        // all dropped before the first flush, so each flush below takes only
        // the one shard it writes. Holding every shard across a multi-CF flush
        // would be the vault-wide lock #1950 removed, wearing a new name.
        let cfs = self
            .read_all_shards()?
            .iter()
            .flat_map(|shard| shard.memtables.iter())
            .filter_map(|(cf, table)| (!table.is_empty()).then_some(*cf))
            .collect::<Vec<_>>();
        self.flush_pending_cfs_at(&cfs, commit_watermark)
    }

    /// Flushes only the selected non-empty memtables at `commit_watermark`.
    ///
    /// Tombstone purges use this to freeze the exact router prefix that their
    /// complete durable compaction must cover. Leaving those rows mutable lets
    /// the durable compaction remove a tombstone while a later process-close
    /// flush writes the same tombstone back as a router-only row.
    pub(crate) fn flush_pending_cfs_at(
        &self,
        cfs: &[ColumnFamily],
        commit_watermark: u64,
    ) -> Result<Vec<SstSummary>> {
        let mut cfs = cfs.to_vec();
        cfs.sort();
        cfs.dedup();
        let mut non_empty = Vec::with_capacity(cfs.len());
        for cf in cfs {
            if self
                .read_shard(cf)?
                .memtables
                .get(&cf)
                .is_some_and(|table| !table.is_empty())
            {
                non_empty.push(cf);
            }
        }
        let cfs = non_empty;
        let mut summaries = Vec::with_capacity(cfs.len());
        for cf in cfs {
            summaries.push(self.flush_cf_at(cf, commit_watermark)?);
        }
        Ok(summaries)
    }
}
