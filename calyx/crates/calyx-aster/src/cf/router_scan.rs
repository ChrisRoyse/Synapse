use super::ColumnFamily;
use super::router::{CfRouter, RouterShard};
use crate::sst::SstEntry;
use crate::sst::level::SstLevel;
use calyx_core::CalyxError;

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
        mut overlay: Vec<SstEntry>,
    ) -> (SstLevel, Vec<SstEntry>) {
        for table in shard.read_tables_oldest_first(cf) {
            overlay.extend(
                table
                    .iter()
                    .filter(|(key, _)| key.as_slice() >= start)
                    .filter(|(key, _)| end.is_none_or(|end| key.as_slice() < end))
                    .map(|(key, value)| SstEntry { key, value }),
            );
        }
        (shard.levels.get(&cf).cloned().unwrap_or_default(), overlay)
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
        // The shard read guard is held across the whole page walk, not just
        // the level clone. `retire_then_purge_cf_inputs` unlinks retired SSTs
        // only after taking this CF's shard for write, and its safety argument
        // is precisely that "readers hold the read lock across their SST reads,
        // so taking the write lock drains every in-flight mapping". Paging a
        // cloned level outside the guard would break that: the clone still
        // names files the purge is free to delete.
        //
        // The hold is now scoped to one column family instead of the whole
        // vault, which is the improvement; making it shorter than the reads it
        // protects would not be one (#1806, #1950).
        let shard = self.read_shard(cf).map_err(E::from)?;
        let (level, overlay) = Self::range_page_sources(&shard, cf, start, end, overlay);
        level.range_pages_with_overlay(start, end, None, limit, overlay, |entries| {
            on_page(self.open_entries(cf, entries).map_err(E::from)?)
        })
    }
}
