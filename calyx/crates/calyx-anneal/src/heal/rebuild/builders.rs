use std::path::PathBuf;

use calyx_aster::cf::{ColumnFamily, slot_key};
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{CalyxError, Clock, PanelSlotId, Result};

use crate::{ArtifactPtr, BudgetHandle};

use super::artifact::{RawSourceRows, artifact_bytes, artifact_hash, source_rows, write_artifact};
use super::{AsterRebuildSource, MvccSnapshot, RebuildTarget, Rebuilder, invalid_target};

pub struct AnnIndexRebuilder<'a, C>
where
    C: Clock,
{
    source: AsterRebuildSource<'a, C>,
    artifact_dir: PathBuf,
    derived_probe: Option<ColumnFamily>,
}

impl<'a, C> AnnIndexRebuilder<'a, C>
where
    C: Clock,
{
    pub fn new(source: AsterRebuildSource<'a, C>, artifact_dir: impl Into<PathBuf>) -> Self {
        Self {
            source,
            artifact_dir: artifact_dir.into(),
            derived_probe: None,
        }
    }

    pub fn with_derived_probe_for_test(mut self, cf: ColumnFamily) -> Self {
        self.derived_probe = Some(cf);
        self
    }
}

impl<C> Rebuilder for AnnIndexRebuilder<'_, C>
where
    C: Clock,
{
    fn rebuild(
        &self,
        target: &RebuildTarget,
        snapshot: MvccSnapshot,
        budget: &mut BudgetHandle,
    ) -> Result<ArtifactPtr> {
        let RebuildTarget::AnnIndex { panel_slot } = target else {
            return Err(invalid_target("AnnIndexRebuilder received non-ANN target"));
        };
        if let Some(cf) = self.derived_probe {
            self.source.scan_cf(snapshot, cf)?;
        }
        let (base_rows, slot_rows) = qualified_ann_source_rows(self.source, snapshot, *panel_slot)?;
        let rows = source_rows(vec![("base", base_rows), ("slot", slot_rows)], budget)?;
        let bytes = artifact_bytes("ann_index_v1", target, snapshot, &rows)?;
        write_artifact(&self.artifact_dir, "ann", target, &bytes).map(ArtifactPtr::HnswGraphPath)
    }
}

fn qualified_ann_source_rows<C>(
    source: AsterRebuildSource<'_, C>,
    snapshot: MvccSnapshot,
    panel_slot: PanelSlotId,
) -> Result<(RawSourceRows, RawSourceRows)>
where
    C: Clock,
{
    let mut base_rows = Vec::new();
    let mut slot_rows = Vec::new();
    for (key, value) in source.scan_cf(snapshot, ColumnFamily::Base)? {
        let cx = decode_constellation_base(&value)?;
        if cx.panel_version != panel_slot.panel_version()
            || !cx.slots.contains_key(&panel_slot.slot_id())
        {
            continue;
        }
        if key.as_slice() != cx.cx_id.as_bytes() {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "Base key does not match constellation {} while rebuilding {panel_slot}",
                cx.cx_id
            )));
        }
        let slot_row_key = slot_key(cx.cx_id);
        let slot_value = source
            .read_cf(
                snapshot,
                ColumnFamily::slot(panel_slot.slot_id()),
                &slot_row_key,
            )?
            .ok_or_else(|| {
                CalyxError::aster_corrupt_shard(format!(
                    "physical slot row missing for {panel_slot} cx_id {}",
                    cx.cx_id
                ))
            })?;
        base_rows.push((key, value));
        slot_rows.push((slot_row_key, slot_value));
    }
    Ok((base_rows, slot_rows))
}

pub struct KernelIndexRebuilder<'a, C>
where
    C: Clock,
{
    source: AsterRebuildSource<'a, C>,
}

impl<'a, C> KernelIndexRebuilder<'a, C>
where
    C: Clock,
{
    pub const fn new(source: AsterRebuildSource<'a, C>) -> Self {
        Self { source }
    }
}

impl<C> Rebuilder for KernelIndexRebuilder<'_, C>
where
    C: Clock,
{
    fn rebuild(
        &self,
        target: &RebuildTarget,
        snapshot: MvccSnapshot,
        budget: &mut BudgetHandle,
    ) -> Result<ArtifactPtr> {
        let RebuildTarget::KernelIndex { .. } = target else {
            return Err(invalid_target(
                "KernelIndexRebuilder received non-kernel target",
            ));
        };
        let rows = source_rows(
            vec![("base", self.source.scan_cf(snapshot, ColumnFamily::Base)?)],
            budget,
        )?;
        Ok(ArtifactPtr::QuantLevelRecordHash(artifact_hash(
            "kernel_index_v1",
            target,
            snapshot,
            &rows,
        )?))
    }
}

pub struct GuardProfileRebuilder<'a, C>
where
    C: Clock,
{
    source: AsterRebuildSource<'a, C>,
}

impl<'a, C> GuardProfileRebuilder<'a, C>
where
    C: Clock,
{
    pub const fn new(source: AsterRebuildSource<'a, C>) -> Self {
        Self { source }
    }
}

impl<C> Rebuilder for GuardProfileRebuilder<'_, C>
where
    C: Clock,
{
    fn rebuild(
        &self,
        target: &RebuildTarget,
        snapshot: MvccSnapshot,
        budget: &mut BudgetHandle,
    ) -> Result<ArtifactPtr> {
        let RebuildTarget::GuardProfile { .. } = target else {
            return Err(invalid_target(
                "GuardProfileRebuilder received non-guard target",
            ));
        };
        let rows = source_rows(
            vec![(
                "anchors",
                self.source.scan_cf(snapshot, ColumnFamily::Anchors)?,
            )],
            budget,
        )?;
        Ok(ArtifactPtr::ConfigCacheKeyHash(artifact_hash(
            "guard_profile_v1",
            target,
            snapshot,
            &rows,
        )?))
    }
}
