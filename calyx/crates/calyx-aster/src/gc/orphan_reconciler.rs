//! PH58 orphan slot/index reconciler.

use crate::cf::{ColumnFamily, base_key, slot_key};
use crate::vault::AsterVault;
use crate::vault::base_rewrite::BaseRowRewrite;
use crate::vault::encode::decode_constellation_base;
use calyx_core::{CalyxError, Clock, CxId, Result, SlotId};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::{Mutex, atomic::AtomicU64};
use std::time::Duration;

pub const CALYX_ORPHAN_RECONCILER_ERROR: &str = "CALYX_ORPHAN_RECONCILER_ERROR";
const REBUILD_PREFIX: &[u8] = b"orphan_slot_rebuild\0";
const REBUILD_METADATA_KEY: &str = "gc.orphan_reconciler";
const REBUILD_METADATA_VALUE: &str = "slot_rebuild_queued";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanBaseEntry {
    pub cx_id: CxId,
    pub expected_slots: Vec<SlotId>,
    pub repair_queued: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OrphanIndexEntry {
    pub cx_id: CxId,
    pub slot: SlotId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrphanReport {
    pub orphan_index: Vec<CxId>,
    pub orphan_base: Vec<CxId>,
    pub orphan_index_entries: Vec<OrphanIndexEntry>,
    pub inconsistencies: usize,
}

impl OrphanReport {
    pub fn to_metrics_text(&self, vault_label: &str) -> String {
        let vault = escape_label(vault_label);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "calyx_orphan_index_entries_total{{vault=\"{vault}\"}} {}",
            self.orphan_index_entries.len()
        );
        let _ = writeln!(
            out,
            "calyx_orphan_base_entries_total{{vault=\"{vault}\"}} {}",
            self.orphan_base.len()
        );
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanRepairResult {
    pub orphan_index_repaired: usize,
    pub orphan_base_degraded: usize,
    pub repairs_total: u64,
    pub remaining_inconsistencies: usize,
    pub rate_limited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanIndexRepair {
    pub cx_id: CxId,
    pub slots: Vec<SlotId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrphanIndexRepairOutcome {
    pub cx_id: CxId,
    pub purged_rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrphanBaseRepairOutcome {
    pub cx_id: CxId,
    pub degraded: bool,
}

impl OrphanRepairResult {
    pub fn to_metrics_text(&self, vault_label: &str) -> String {
        let vault = escape_label(vault_label);
        format!(
            "calyx_orphan_repairs_total{{vault=\"{vault}\"}} {}\n",
            self.repairs_total
        )
    }
}

pub trait OrphanGcTarget {
    fn base_entries(&self) -> Result<Vec<OrphanBaseEntry>>;
    fn slot_index_entries(&self) -> Result<Vec<OrphanIndexEntry>>;
    fn purge_orphan_index(&self, cx_id: CxId, slots: &[SlotId]) -> Result<usize>;
    fn flag_orphan_base(&self, cx_id: CxId) -> Result<()>;

    fn purge_orphan_indexes(
        &self,
        repairs: &[OrphanIndexRepair],
    ) -> Result<Vec<OrphanIndexRepairOutcome>> {
        repairs
            .iter()
            .map(|repair| {
                self.purge_orphan_index(repair.cx_id, &repair.slots)
                    .map(|purged_rows| OrphanIndexRepairOutcome {
                        cx_id: repair.cx_id,
                        purged_rows,
                    })
            })
            .collect()
    }

    fn flag_orphan_bases(&self, cx_ids: &[CxId]) -> Result<Vec<OrphanBaseRepairOutcome>> {
        cx_ids
            .iter()
            .map(|cx_id| {
                self.flag_orphan_base(*cx_id)
                    .map(|()| OrphanBaseRepairOutcome {
                        cx_id: *cx_id,
                        degraded: true,
                    })
            })
            .collect()
    }

    /// Finishes the index-repair phase after every durable chunk has committed.
    /// Real storage targets use this hook to compact each distinct affected CF
    /// once for the entire run. Implementations must retain retryable state if
    /// finalization fails.
    fn finish_orphan_index_repairs(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct OrphanReconciler {
    pub scan_interval: Duration,
    pub max_repairs_per_run: usize,
    orphan_repairs_total: AtomicU64,
}

impl OrphanReconciler {
    pub fn new(scan_interval: Duration, max_repairs_per_run: usize) -> Self {
        Self {
            scan_interval,
            max_repairs_per_run,
            orphan_repairs_total: AtomicU64::new(0),
        }
    }

    pub fn scan<T>(&self, target: &T) -> Result<OrphanReport>
    where
        T: OrphanGcTarget + ?Sized,
    {
        let base_entries = target.base_entries()?;
        let slot_entries = target.slot_index_entries()?;
        let base_by_cx = base_entries
            .into_iter()
            .map(|entry| (entry.cx_id, entry))
            .collect::<BTreeMap<_, _>>();
        let slot_by_cx = slot_entries.iter().fold(
            BTreeMap::<CxId, BTreeSet<SlotId>>::new(),
            |mut acc, entry| {
                acc.entry(entry.cx_id).or_default().insert(entry.slot);
                acc
            },
        );

        let mut orphan_index = BTreeSet::new();
        let mut orphan_index_entries = Vec::new();
        for entry in slot_entries {
            if !base_by_cx.contains_key(&entry.cx_id) {
                orphan_index.insert(entry.cx_id);
                orphan_index_entries.push(entry);
            }
        }

        let mut orphan_base = Vec::new();
        for (cx_id, base) in &base_by_cx {
            if base.expected_slots.is_empty() || base.repair_queued {
                continue;
            }
            let has_any_expected = slot_by_cx
                .get(cx_id)
                .is_some_and(|slots| base.expected_slots.iter().any(|slot| slots.contains(slot)));
            if !has_any_expected {
                orphan_base.push(*cx_id);
            }
        }

        orphan_index_entries.sort_unstable();
        let orphan_index = orphan_index.into_iter().collect::<Vec<_>>();
        let inconsistencies = orphan_index.len() + orphan_base.len();
        Ok(OrphanReport {
            orphan_index,
            orphan_base,
            orphan_index_entries,
            inconsistencies,
        })
    }
}

impl Default for OrphanReconciler {
    fn default() -> Self {
        Self::new(Duration::from_secs(300), 1_000)
    }
}

pub struct VaultOrphanGcTarget<'a, C> {
    vault: &'a AsterVault<C>,
    slots: Vec<SlotId>,
    compact_after_tombstone: bool,
    pending_compaction_cfs: Mutex<BTreeSet<ColumnFamily>>,
}

impl<'a, C> VaultOrphanGcTarget<'a, C> {
    pub fn new(vault: &'a AsterVault<C>, slots: impl IntoIterator<Item = SlotId>) -> Self {
        let mut slots = slots.into_iter().collect::<Vec<_>>();
        slots.sort_unstable_by_key(|slot| slot.get());
        slots.dedup();
        Self {
            vault,
            slots,
            compact_after_tombstone: true,
            pending_compaction_cfs: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn without_auto_compaction(mut self) -> Self {
        self.compact_after_tombstone = false;
        self
    }
}

fn key_to_cx(key: &[u8]) -> Result<CxId> {
    let bytes: [u8; 16] = key
        .try_into()
        .map_err(|_| orphan_error("slot/base key is not a 16-byte CxId"))?;
    Ok(CxId::from_bytes(bytes))
}

fn orphan_payload(event: &str, count: usize) -> Result<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({ "event": event, "count": count }))
        .map_err(|error| orphan_error(format!("encode orphan repair ledger payload: {error}")))
}

fn orphan_error(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ORPHAN_RECONCILER_ERROR,
        message: message.into(),
        remediation: "rerun orphan scan, inspect base/slot CF bytes, and repair from WAL",
    }
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

mod repair;
mod vault_target;
