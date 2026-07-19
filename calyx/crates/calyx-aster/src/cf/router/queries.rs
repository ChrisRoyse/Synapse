use super::*;

impl CfRouter {
    pub fn get(&self, cf: ColumnFamily, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.memtables.get(&cf).and_then(|table| table.get(key)) {
            return Ok(Some(value));
        }
        self.levels.get(&cf).map_or(Ok(None), |level| {
            level
                .get(key)?
                .map(|value| self.open_value(cf, key, value))
                .transpose()
        })
    }

    pub fn range(&self, cf: ColumnFamily, start: &[u8], end: &[u8]) -> Result<Vec<SstEntry>> {
        let mut rows = BTreeMap::new();
        if let Some(level) = self.levels.get(&cf) {
            for entry in level.range(start, end)? {
                rows.insert(entry.key, entry.value);
            }
        }
        if let Some(table) = self.memtables.get(&cf) {
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
        let mut upper = upper.to_vec();
        let mut inclusive = inclusive;
        loop {
            let level = self
                .levels
                .get(&cf)
                .map(|level| level.predecessor(start, &upper, inclusive))
                .transpose()?
                .flatten();
            let memtable = self
                .memtables
                .get(&cf)
                .and_then(|table| table.predecessor(start, &upper, inclusive))
                .map(|(key, value)| SstEntry { key, value });
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
            let value = self.open_value(cf, &key, value)?;
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
        let overlay = self
            .memtables
            .get(&cf)
            .map(|table| {
                table
                    .range_until(start, end)
                    .into_iter()
                    .map(|(key, value)| SstEntry { key, value })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let rows = self
            .levels
            .get(&cf)
            .cloned()
            .unwrap_or_default()
            .range_page_with_overlay(start, end, after_key, limit, overlay)?;
        self.open_entries(cf, rows)
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
        let immutable = self
            .levels
            .get(&cf)
            .map(|level| level.range_candidate_page_until(start, end, after_key, limit))
            .transpose()?
            .unwrap_or_default();
        // Mutable router rows are plaintext until flush; immutable SST rows
        // may be sealed. Open only the immutable source before merging so an
        // encrypted vault never tries to decrypt a plaintext memtable winner.
        let immutable = self.open_entries(cf, immutable)?;
        let mutable = self
            .memtables
            .get(&cf)
            .map(|table| {
                table
                    .range_candidate_page_until(start, end, after_key, limit)
                    .into_iter()
                    .map(|(key, value)| SstEntry { key, value })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

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
        let mut rows = BTreeMap::<Vec<u8>, bool>::new();
        if let Some(level) = self.levels.get(&cf) {
            for key in level.range_keys_until(start, end)? {
                rows.insert(key, false);
            }
        }
        if let Some(table) = self.memtables.get(&cf) {
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
        let mut rows = BTreeMap::new();
        if let Some(level) = self.levels.get(&cf) {
            for entry in level.iter()? {
                rows.insert(entry.key, entry.value);
            }
        }
        if let Some(table) = self.memtables.get(&cf) {
            for (key, value) in table.iter() {
                rows.insert(key, value);
            }
        }
        self.open_entries(
            cf,
            rows.into_iter().map(|(key, value)| SstEntry { key, value }),
        )
    }

    pub fn level_file_count(&self, cf: ColumnFamily) -> usize {
        self.levels.get(&cf).map_or(0, SstLevel::file_count)
    }

    /// Raw flush with no commit domain; see [`Self::flush_pending_at`].
    pub fn flush_pending(&mut self) -> Result<Vec<SstSummary>> {
        self.flush_pending_at(NO_COMMIT_DOMAIN)
    }

    /// Flushes every non-empty memtable at `commit_watermark`; see
    /// [`Self::flush_cf_at`] for the watermark contract.
    pub fn flush_pending_at(&mut self, commit_watermark: u64) -> Result<Vec<SstSummary>> {
        let cfs = self
            .memtables
            .iter()
            .filter_map(|(cf, table)| (!table.is_empty()).then_some(*cf))
            .collect::<Vec<_>>();
        let mut summaries = Vec::with_capacity(cfs.len());
        for cf in cfs {
            summaries.push(self.flush_cf_at(cf, commit_watermark)?);
        }
        Ok(summaries)
    }
}
