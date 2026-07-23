//! Orphan physical slot-CF retirement.
//!
//! Pre-`PanelSlotId` write paths left physical `cf/slot_*` column families that
//! no live panel references (issue #1776, e.g. `slot_101..115`). Because the
//! ratified identity commits `panel_version` into every Base `CxId`, a physical
//! slot CF is orphaned exactly when no live Base row references its `SlotId`:
//! every live constellation only writes its own panel's slots, so the union of
//! slot ids across all live Base rows is the authoritative legitimate set. This
//! module derives that set from physical truth (never hardcoding a slot range),
//! then retires the orphans fail-closed with readback, holding the durable
//! maintenance lock only for one CF drop at a time to bound lock holds (#1806).

use std::collections::BTreeSet;

use calyx_core::{CalyxError, Clock, Result};
use serde::Serialize;

use super::AsterVault;
use super::compaction_bridge::try_acquire_native_compaction_guard;
use super::encode::decode_constellation_base;
use crate::cf::ColumnFamily;

/// Outcome of one orphan slot-CF retirement pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AsterOrphanSlotGcReport {
    /// Base rows scanned to derive the legitimate slot set.
    pub base_rows_scanned: usize,
    /// Slot ids referenced by at least one live Base row (the legitimate set).
    pub live_slot_ids: Vec<u16>,
    /// Physical `cf/slot_*` ids discovered on disk.
    pub present_slot_ids: Vec<u16>,
    /// Orphan slot CFs that were retired this pass.
    pub retired: Vec<AsterOrphanSlotCfRetirement>,
    /// Candidate orphans left in place because a key resolved to a live Base
    /// row (fail-closed) — this must be empty on a healthy vault.
    pub skipped_live: Vec<AsterOrphanSlotCfSkip>,
}

/// One retired orphan slot CF (quantized column plus its raw sidecar).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AsterOrphanSlotCfRetirement {
    pub slot_id: u16,
    pub quantized_rows: usize,
    pub quantized_sst_files: usize,
    pub raw_rows: usize,
    pub raw_sst_files: usize,
    pub removed_dirs: Vec<String>,
}

/// A candidate orphan that was refused because it still holds a live reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AsterOrphanSlotCfSkip {
    pub slot_id: u16,
    pub reason: String,
}

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Retires orphaned physical slot column families.
    ///
    /// A slot CF is orphaned when no live Base row references its `SlotId`. Each
    /// candidate's rows are re-checked against the live Base key set and the
    /// pass fails closed (leaves the CF in place, records it) if any key resolves
    /// to a live Base row. Retirement is idempotent (an absent CF is skipped),
    /// logs exact row/SST counts, and readback-verifies the directory is gone.
    ///
    /// The durable maintenance lock is acquired per CF drop, so a pass retiring
    /// several CFs never holds the cross-process lock across all of them.
    ///
    /// # Errors
    ///
    /// Fails closed when the vault has no durable maintenance context, a Base
    /// row cannot be decoded, a scan fails, the lock cannot be acquired, or the
    /// physical removal/readback fails.
    pub fn retire_orphan_slot_cfs(&self) -> Result<AsterOrphanSlotGcReport> {
        if self.durable.is_none() {
            return Err(CalyxError::aster_corrupt_shard(
                "orphan slot-CF retirement requires a durable maintenance vault",
            ));
        }
        // Derive the legitimate slot set + live Base key set from physical truth.
        let base = self.scan_cf_latest(ColumnFamily::Base)?;
        let base_rows_scanned = base.len();
        let mut live_slot_ids = BTreeSet::new();
        let mut live_base_keys = BTreeSet::new();
        for (key, value) in &base {
            live_base_keys.insert(key.clone());
            let constellation = decode_constellation_base(value)?;
            live_slot_ids.extend(constellation.slots.keys().map(|slot| slot.get()));
        }

        let present = self.rows.present_slot_cf_ids()?;
        let present_slot_ids: Vec<u16> = present.iter().map(|slot| slot.get()).collect();
        tracing::info!(
            code = "CALYX_ASTER_ORPHAN_SLOT_GC_START",
            base_rows_scanned,
            live_slot_id_count = live_slot_ids.len(),
            present_slot_cf_count = present.len(),
            "starting orphan slot-CF retirement pass"
        );

        let mut retired = Vec::new();
        let mut skipped_live = Vec::new();
        for slot in &present {
            if live_slot_ids.contains(&slot.get()) {
                // Referenced by a live panel's Base rows — legitimate, keep.
                continue;
            }
            let quantized_cf = ColumnFamily::slot(*slot);
            let raw_cf = ColumnFamily::slot_raw(*slot);
            let quantized_rows = self.scan_cf_latest(quantized_cf)?;
            let raw_rows = self.scan_cf_latest(raw_cf)?;

            // Fail-closed defense in depth: no key may resolve to a live Base row.
            if let Some((key, _)) = quantized_rows
                .iter()
                .chain(raw_rows.iter())
                .find(|(key, _)| live_base_keys.contains(key))
            {
                let reason = format!(
                    "orphan candidate slot {} holds a row keyed by live Base CxId {}; refusing to retire",
                    slot.get(),
                    hex(key)
                );
                tracing::warn!(
                    code = "CALYX_ASTER_ORPHAN_SLOT_CF_LIVE_REFERENCE",
                    slot_id = slot.get(),
                    live_cx_id = %hex(key),
                    "refusing to retire a slot CF that still resolves to a live Base row"
                );
                skipped_live.push(AsterOrphanSlotCfSkip {
                    slot_id: slot.get(),
                    reason,
                });
                continue;
            }

            tracing::info!(
                code = "CALYX_ASTER_ORPHAN_SLOT_CF_RETIRE_PLANNED",
                slot_id = slot.get(),
                quantized_rows = quantized_rows.len(),
                raw_rows = raw_rows.len(),
                "retiring orphan slot CF (no live Base row references this slot)"
            );

            // Hold the cross-process durable maintenance lock only for this one
            // slot's drop, then release before the next candidate (#1806).
            let quantized_physical;
            let raw_physical;
            {
                let durable = self.durable.as_ref().ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(
                        "orphan slot-CF retirement lost its durable maintenance vault",
                    )
                })?;
                let _maintenance_guard = try_acquire_native_compaction_guard(durable)?;
                quantized_physical = self
                    .rows
                    .retire_router_cf(quantized_cf, "retire orphan slot CF")?;
                raw_physical = self
                    .rows
                    .retire_router_cf(raw_cf, "retire orphan raw slot sidecar CF")?;
            }

            let mut removed_dirs: Vec<String> = quantized_physical
                .removed_dirs
                .iter()
                .chain(raw_physical.removed_dirs.iter())
                .map(|path| path.display().to_string())
                .collect();
            removed_dirs.sort();
            tracing::info!(
                code = "CALYX_ASTER_ORPHAN_SLOT_CF_RETIRED",
                slot_id = slot.get(),
                quantized_rows = quantized_rows.len(),
                quantized_sst_files = quantized_physical.removed_sst_files,
                raw_rows = raw_rows.len(),
                raw_sst_files = raw_physical.removed_sst_files,
                removed_dir_count = removed_dirs.len(),
                "retired orphan slot CF with readback-verified physical removal"
            );
            retired.push(AsterOrphanSlotCfRetirement {
                slot_id: slot.get(),
                quantized_rows: quantized_rows.len(),
                quantized_sst_files: quantized_physical.removed_sst_files,
                raw_rows: raw_rows.len(),
                raw_sst_files: raw_physical.removed_sst_files,
                removed_dirs,
            });
        }

        tracing::info!(
            code = "CALYX_ASTER_ORPHAN_SLOT_GC_DONE",
            base_rows_scanned,
            retired_count = retired.len(),
            skipped_live_count = skipped_live.len(),
            "completed orphan slot-CF retirement pass"
        );
        Ok(AsterOrphanSlotGcReport {
            base_rows_scanned,
            live_slot_ids: live_slot_ids.into_iter().collect(),
            present_slot_ids,
            retired,
            skipped_live,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
