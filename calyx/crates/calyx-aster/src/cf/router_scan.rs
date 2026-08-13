use super::router::{CfRouter, RouterShard};
use super::{ColumnFamily, KeyRange};
use crate::sst::SstEntry;
use crate::sst::level::SstLevel;
use calyx_core::CalyxError;
use std::collections::BTreeMap;

impl CfRouter {
    /// The immutable level and the mutable overlay for one CF's range page.
    ///
    /// Takes a shard guard the caller already holds rather than acquiring one.
    /// `std::sync::RwLock` is not reentrant, so acquiring here would deadlock
    /// any caller that is already inside the shard (#1950).
    ///
    /// Merges **every** in-memory source oldest-first, not only the active
    /// memtable. A sealed-but-not-yet-installed memtable holds the newest state
    /// for its keys until its SST lands, so omitting it served stale rows for
    /// the whole duration of a flush — the window #1949 widened when it moved
    /// the SST write out from under the locks, and the one contract
    /// `RouterShard::read_tables_oldest_first` exists to keep.
    pub(super) fn range_page_sources(
        shard: &RouterShard,
        cf: ColumnFamily,
        start: &[u8],
        end: Option<&[u8]>,
        overlay: Vec<SstEntry>,
    ) -> (SstLevel, Vec<SstEntry>) {
        let mut merged = BTreeMap::new();
        for table in shard.read_tables_oldest_first(cf) {
            for (key, value) in table
                .iter()
                .filter(|(key, _)| key.as_slice() >= start)
                .filter(|(key, _)| end.is_none_or(|end| key.as_slice() < end))
            {
                merged.insert(key, value);
            }
        }
        // A caller-supplied overlay is the newest logical source. Folding it
        // through the map also makes duplicate precedence deterministic before
        // the SST cursor sees the rows.
        for entry in overlay {
            merged.insert(entry.key, entry.value);
        }
        (
            shard.levels.get(&cf).cloned().unwrap_or_default(),
            merged
                .into_iter()
                .map(|(key, value)| SstEntry { key, value })
                .collect(),
        )
    }

    /// # Errors
    ///
    /// Returns the caller's error type on a page-callback failure, or a
    /// corrupt-shard error when the shard owning `cf` is poisoned.
    pub fn range_pages_until<F, E>(
        &self,
        cf: ColumnFamily,
        start: &[u8],
        end: Option<&[u8]>,
        limit: usize,
        overlay: Vec<SstEntry>,
        mut on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<SstEntry>) -> std::result::Result<(), E>,
        E: From<CalyxError>,
    {
        if limit == 0 {
            return Ok(());
        }
        let shard = self.read_shard(cf).map_err(E::from)?;
        let (level, overlay) = Self::range_page_sources(&shard, cf, start, end, overlay);
        // Open every immutable handle while retirement is excluded, then let
        // the owning stream carry those handles and Arc-backed lookup metadata
        // beyond the guard. A retired path may be unlinked afterwards, but an
        // already-open handle remains the same immutable file on both Unix and
        // Windows. Callbacks therefore run without a router lock and may safely
        // hydrate or write related rows (#1806, #1950).
        let mut stream = level
            .open_page_stream_with_overlay_origins(start, end, None, limit, overlay)
            .map_err(E::from)?;
        drop(shard);
        while let Some(winners) = stream.next_page().map_err(E::from)? {
            on_page(self.open_page_winners(cf, winners).map_err(E::from)?)?;
        }
        Ok(())
    }

    /// Streams one immutable CF view merged with an exact plaintext MVCC
    /// overlay, preserving one SST cursor for the whole walk.
    ///
    /// `release_row_guard` is invoked only after the CF shard read guard has
    /// been acquired. The caller uses that hand-off to preserve the global
    /// rows -> router lock order while closing the race between collecting the
    /// snapshot overlay and pinning the immutable file set. Mutable router
    /// tables are deliberately excluded: every key changed since recovery is
    /// represented by the MVCC overlay at the pinned sequence, so consulting a
    /// latest-only memtable would reintroduce post-snapshot state.
    pub(crate) fn range_immutable_pages_until<F, E, R>(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        limit: usize,
        overlay: Vec<SstEntry>,
        release_row_guard: R,
        mut on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<SstEntry>) -> std::result::Result<(), E>,
        E: From<CalyxError>,
        R: FnOnce(),
    {
        if limit == 0 {
            release_row_guard();
            return Ok(());
        }
        let shard = self.read_shard(cf).map_err(E::from)?;
        // The caller still holds the row-table guard here. Acquiring the
        // router first and releasing the row guard only now makes this one
        // atomic lock hand-off, in the same order every commit uses.
        release_row_guard();
        let level = shard.levels.get(&cf).cloned().unwrap_or_default();
        let mut stream = level
            .open_page_stream_with_overlay_origins(
                &range.start,
                range.end.as_deref(),
                None,
                limit,
                overlay,
            )
            .map_err(E::from)?;
        drop(shard);
        while let Some(winners) = stream.next_page().map_err(E::from)? {
            on_page(self.open_page_winners(cf, winners).map_err(E::from)?)?;
        }
        Ok(())
    }

    /// Streams exact newest-wins key/tombstone state without opening values.
    ///
    /// The lock hand-off is identical to `range_immutable_pages_until`, but
    /// every immutable record is CRC-validated sequentially and only its key
    /// state crosses the router boundary. This is the physical primitive for
    /// exact counts; it must never decrypt or materialize payloads.
    pub(crate) fn range_immutable_key_state_pages_until<F, E, R>(
        &self,
        cf: ColumnFamily,
        range: &KeyRange,
        limit: usize,
        overlay: Vec<SstEntry>,
        release_row_guard: R,
        mut on_page: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(Vec<crate::sst::page::SstKeyState>) -> std::result::Result<(), E>,
        E: From<CalyxError>,
        R: FnOnce(),
    {
        if limit == 0 {
            release_row_guard();
            return Ok(());
        }
        let shard = self.read_shard(cf).map_err(E::from)?;
        release_row_guard();
        let level = shard.levels.get(&cf).cloned().unwrap_or_default();
        let mut stream = level
            .open_key_state_page_stream_with_overlay(
                &range.start,
                range.end.as_deref(),
                limit,
                overlay,
            )
            .map_err(E::from)?;
        drop(shard);
        while let Some(states) = stream.next_page().map_err(E::from)? {
            on_page(states)?;
        }
        Ok(())
    }

    fn open_page_winners(
        &self,
        cf: ColumnFamily,
        winners: Vec<crate::sst::page::SstPageWinner>,
    ) -> Result<Vec<SstEntry>, CalyxError> {
        let mut entries = Vec::with_capacity(winners.len());
        for winner in winners {
            let entry = if winner.from_overlay {
                winner.entry
            } else {
                let mut opened = self.open_entries(cf, [winner.entry])?;
                opened.pop().ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "opening one immutable {} page row returned no row",
                        cf.name()
                    ))
                })?
            };
            if !crate::mvcc::is_tombstone_value(&entry.value) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }
}
