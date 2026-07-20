use crate::cf::{ColumnFamily, SlotFamilyKind};
use crate::sst::write_sst;
use calyx_core::{CalyxError, Result, SlotId};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Hot/cold physical storage tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageTier {
    Hot,
    Cold,
}

/// Resolved destination for one CF write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierPlacement {
    pub tier: StorageTier,
    pub root: PathBuf,
    pub cf_dir: PathBuf,
}

impl TierPlacement {
    pub fn absolute_dir(&self) -> PathBuf {
        self.root.join(&self.cf_dir)
    }
}

/// PH11 tiering policy.
#[derive(Debug, Clone)]
pub struct TieringPolicy {
    hot_root: PathBuf,
    archive_root: PathBuf,
    active_slots: BTreeSet<SlotId>,
    current_panel_version: u32,
}

impl TieringPolicy {
    pub fn new(
        hot_root: impl Into<PathBuf>,
        archive_root: impl Into<PathBuf>,
        active_slots: impl IntoIterator<Item = SlotId>,
        current_panel_version: u32,
    ) -> Self {
        Self {
            hot_root: hot_root.into(),
            archive_root: archive_root.into(),
            active_slots: active_slots.into_iter().collect(),
            current_panel_version,
        }
    }

    pub fn manual(
        active_slots: impl IntoIterator<Item = SlotId>,
        current_panel_version: u32,
    ) -> Self {
        Self::new(
            tier_root("/zfs/hot/calyx", "hot"),
            tier_root("/zfs/archive/calyx", "archive"),
            active_slots,
            current_panel_version,
        )
    }

    pub fn hot_root(&self) -> &PathBuf {
        &self.hot_root
    }

    pub fn archive_root(&self) -> &PathBuf {
        &self.archive_root
    }

    pub fn tier_roots(&self) -> Vec<PathBuf> {
        if self.hot_root == self.archive_root {
            vec![self.hot_root.clone()]
        } else {
            vec![self.hot_root.clone(), self.archive_root.clone()]
        }
    }

    pub fn place_current_cf(&self, cf: ColumnFamily) -> TierPlacement {
        self.place_cf(cf, self.current_panel_version)
    }

    pub fn place_cf(&self, cf: ColumnFamily, panel_version: u32) -> TierPlacement {
        let cold = self.is_cold(cf, panel_version);
        let root = if cold {
            self.archive_root.clone()
        } else {
            self.hot_root.clone()
        };
        TierPlacement {
            tier: if cold {
                StorageTier::Cold
            } else {
                StorageTier::Hot
            },
            root,
            cf_dir: PathBuf::from("cf").join(cf.name()),
        }
    }

    pub fn write_tiered_sst<'a>(
        &self,
        cf: ColumnFamily,
        panel_version: u32,
        file_name: &str,
        entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<TierWrite> {
        // A non-canonical name would be invisible to every fail-closed
        // recovery/scan pass, so refuse to create it in the first place.
        if crate::storage_names::classify_sst(Path::new(file_name))?.is_none() {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "tiered SST file name {file_name} is not a canonical Aster SST name"
            )));
        }
        let placement = self.place_cf(cf, panel_version);
        let dir = placement.absolute_dir();
        fs::create_dir_all(&dir)
            .map_err(|error| CalyxError::disk_pressure(format!("create tier dir: {error}")))?;
        let path = dir.join(file_name);
        let summary = write_sst(&path, entries)?;
        Ok(TierWrite {
            placement,
            path: summary.path,
            bytes: summary.bytes,
            staging_parent: dir,
        })
    }

    fn is_cold(&self, cf: ColumnFamily, panel_version: u32) -> bool {
        if matches!(
            cf,
            ColumnFamily::Base
                | ColumnFamily::Ledger
                | ColumnFamily::Anchors
                | ColumnFamily::Graph
                | ColumnFamily::Reactive
                | ColumnFamily::AnnealChecksums
                | ColumnFamily::AnnealMistakes
                | ColumnFamily::AnnealReplay
                | ColumnFamily::AnnealHeads
                | ColumnFamily::AnnealBandit
                | ColumnFamily::AnnealSoak
                | ColumnFamily::AnnealReport
                | ColumnFamily::AnnealGrowth
                | ColumnFamily::AnnealOperators
                | ColumnFamily::Registry
        ) {
            return false;
        }
        if cf.is_raw_slot() {
            return true;
        }
        matches!(
            cf,
            ColumnFamily::Slot {
                slot,
                kind: SlotFamilyKind::Quantized
            } if panel_version < self.current_panel_version || !self.active_slots.contains(&slot)
        )
    }
}

fn tier_root(zfs_path: &str, fallback_dir: &str) -> PathBuf {
    let zfs = PathBuf::from(zfs_path);
    if zfs.exists() {
        return zfs;
    }
    env::var_os("CALYX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/calyx"))
        .join(fallback_dir)
}

/// Completed tiered SST write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierWrite {
    pub placement: TierPlacement,
    pub path: PathBuf,
    pub bytes: u64,
    pub staging_parent: PathBuf,
}
