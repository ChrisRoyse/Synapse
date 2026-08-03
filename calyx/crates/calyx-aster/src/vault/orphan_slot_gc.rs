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

/// Rows per bounded page while deriving the live Base key set (#1968).
///
/// Matches the page size the Synapse-side walker settled on after sweeping the
/// real 106,936-row `Base` CF: total CPU is nearly flat from 256 to 8,192 pages,
/// so a smaller page buys a shorter hold almost for free, and the 25 ms
/// row-guard budget has a cliff between 2,048 and 4,096. 256 lands the worst
/// single hold at roughly 16% of budget.
const ORPHAN_SLOT_GC_PAGE_ROWS: usize = 256;

impl<C> AsterVault<C>
where
    C: Clock,
{
    /// Folds every live `Base` row through `visit` in bounded pages, releasing
    /// the row guard between pages, and fails closed if the vault committed
    /// anything mid-walk (#1968).
    ///
    /// For callers whose decision must be taken against one instant. A paged
    /// walk normally describes an *interval*, which is fine for a census and
    /// wrong for anything destructive; this makes the difference explicit
    /// instead of leaving it to a comment.
    fn walk_base_pages_atomic<F>(
        &self,
        operation: &'static str,
        page_rows: usize,
        mut visit: F,
    ) -> Result<()>
    where
        F: FnMut(&[u8], &[u8]) -> Result<()>,
    {
        if page_rows == 0 {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "{operation} needs a positive page size; zero rows per page cannot make forward \
                 progress"
            )));
        }
        let range = crate::cf::KeyRange::all();
        let mut cursor: Option<Vec<u8>> = None;
        let mut first_seq: Option<u64> = None;
        loop {
            let page = self.scan_cf_range_page_latest(
                ColumnFamily::Base,
                &range,
                cursor.as_deref(),
                page_rows,
            )?;
            let opened_at = *first_seq.get_or_insert(page.snapshot_seq);
            if page.snapshot_seq != opened_at {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "{operation} requires an atomic view of Base: the walk opened at committed \
                     sequence {opened_at} but a later page served sequence {}, so a row committed \
                     mid-walk may be missing from the derived live set. Retry when the vault is \
                     quiescent; this decision must not be taken from a moving window",
                    page.snapshot_seq
                )));
            }
            for (key, value) in &page.rows {
                visit(key, value)?;
            }
            if !page.more {
                return Ok(());
            }
            let Some(resume) = page.resume_after.clone() else {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "{operation} paged Base reported more rows but returned no resume cursor, so \
                     the walk cannot advance"
                )));
            };
            if cursor.as_ref().is_some_and(|previous| *previous >= resume) {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "{operation} paged Base returned a resume cursor that does not advance, which \
                     would re-read the same page forever"
                )));
            }
            cursor = Some(resume);
        }
    }

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
        //
        // Paged rather than materialized (#1968). Unlike the five maintenance
        // folds that motivated that issue, this pass genuinely needs every Base
        // key resident — `live_base_keys` is the fail-closed guard consulted per
        // orphan candidate below — so the win here is only the *hold*, which
        // `scan_cf_latest` kept for the whole 106k-row scan and measured at up to
        // 2.65 s on the CF every constellation write lands in.
        //
        // Paging trades one long atomic view for many short ones, and this pass
        // **deletes column families**. A row committed mid-walk could be missed,
        // and a slot CF referencing it would then look orphaned. So the interval
        // is asserted rather than assumed: `walk_base_pages_atomic` fails closed
        // if any commit lands during the walk. Retrying under write load is the
        // correct cost for a destructive decision; deciding it from a moving
        // window is not.
        let mut base_rows_scanned = 0usize;
        let mut live_slot_ids = BTreeSet::new();
        let mut live_base_keys = BTreeSet::new();
        self.walk_base_pages_atomic(
            "retire_orphan_slot_cfs",
            ORPHAN_SLOT_GC_PAGE_ROWS,
            |key, value| {
                base_rows_scanned += 1;
                live_base_keys.insert(key.to_vec());
                let constellation = decode_constellation_base(value)?;
                live_slot_ids.extend(constellation.slots.keys().map(|slot| slot.get()));
                Ok(())
            },
        )?;

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
            // These stay materialized: the retirement report publishes their exact
            // row counts and the live-reference check below needs every key. They
            // are orphan *candidates*, so they are the CFs no live panel writes —
            // unlike `Base`, nothing is queued behind this hold.
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
