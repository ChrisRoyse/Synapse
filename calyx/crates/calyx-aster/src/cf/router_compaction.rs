use super::{CfRouter, ColumnFamily, NO_COMMIT_DOMAIN};
use crate::mvcc::is_tombstone_value;
use crate::sst::level::SstLevel;
use crate::sst::{invalidate_reader, write_sst};
use crate::storage_names::flush_sst_file_name;
use calyx_core::{CalyxError, Result};
use std::fs;

impl CfRouter {
    /// Collapses each listed CF to one newest router-domain SST and removes
    /// tombstone rows after the replacement is durable.
    pub fn compact_tombstoned_cfs(&self, cfs: &[ColumnFamily]) -> Result<()> {
        self.compact_tombstoned_cfs_at(cfs, NO_COMMIT_DOMAIN)
    }

    pub(crate) fn compact_tombstoned_cfs_at(
        &self,
        cfs: &[ColumnFamily],
        commit_watermark: u64,
    ) -> Result<()> {
        self.flush_pending_at(commit_watermark)?;
        let mut unique = Vec::new();
        for cf in cfs {
            if !unique.contains(cf) {
                unique.push(*cf);
            }
        }
        for cf in unique {
            self.compact_tombstoned_cf(cf, commit_watermark)?;
        }
        Ok(())
    }

    fn compact_tombstoned_cf(&self, cf: ColumnFamily, commit_watermark: u64) -> Result<()> {
        // One shard's write guard, held across the rewrite. The SST write here
        // is genuinely exclusive: it replaces this CF's whole level, so a
        // concurrent seal installing into the old level would be discarded by
        // the swap. Only this column family's shard is held, so every other CF
        // keeps both reading and committing throughout (#1950).
        let mut shard = self.write_shard(cf)?;
        let Some(level) = shard.levels.get(&cf) else {
            return Ok(());
        };
        let input_paths = level.file_paths_newest_first();
        if input_paths.is_empty() {
            return Ok(());
        }
        let visible = level.iter()?;
        let retained = visible
            .into_iter()
            .filter(|entry| !is_tombstone_value(&entry.value))
            .collect::<Vec<_>>();
        let ordinal = usize::try_from(shard.next_sequence(cf)).map_err(|_| {
            CalyxError::aster_corrupt_shard(format!(
                "router compaction ordinal for {} exceeds the platform usize range",
                cf.name()
            ))
        })?;
        let output = self
            .cf_dir(cf)
            .join(flush_sst_file_name(commit_watermark, ordinal));
        let summary = write_sst(
            &output,
            retained
                .iter()
                .map(|entry| (entry.key.as_slice(), entry.value.as_slice())),
        )?;
        let prepared = SstLevel::prepare(&summary, self.config.retains_lookup(cf))?;
        let mut replacement = SstLevel::new();
        replacement.push_prepared(prepared)?;
        shard.levels.insert(cf, replacement);

        for input in input_paths {
            if input == output {
                continue;
            }
            invalidate_reader(&input);
            fs::remove_file(&input).map_err(|error| {
                CalyxError::disk_pressure(format!(
                    "reclaim router compaction input {}: {error}",
                    input.display()
                ))
            })?;
        }
        Ok(())
    }
}
