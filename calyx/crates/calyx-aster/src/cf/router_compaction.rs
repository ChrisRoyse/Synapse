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
    pub fn compact_tombstoned_cfs(&mut self, cfs: &[ColumnFamily]) -> Result<()> {
        self.compact_tombstoned_cfs_at(cfs, NO_COMMIT_DOMAIN)
    }

    pub(crate) fn compact_tombstoned_cfs_at(
        &mut self,
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

    fn compact_tombstoned_cf(&mut self, cf: ColumnFamily, commit_watermark: u64) -> Result<()> {
        let Some(level) = self.levels.get(&cf) else {
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
        let ordinal = usize::try_from(self.next_sequence(cf)).map_err(|_| {
            CalyxError::aster_corrupt_shard(format!(
                "router compaction ordinal for {} exceeds the platform usize range",
                cf.name()
            ))
        })?;
        let output = self
            .cf_dir(cf)
            .join(flush_sst_file_name(commit_watermark, ordinal));
        write_sst(
            &output,
            retained
                .iter()
                .map(|entry| (entry.key.as_slice(), entry.value.as_slice())),
        )?;
        let replacement = SstLevel::from_oldest_first_with_lookup([output.clone()])?;
        self.levels.insert(cf, replacement);

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
