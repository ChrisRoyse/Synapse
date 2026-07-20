//! Concurrent-read-safe SlotId to index registry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use calyx_core::{CxId, Panel, Result, SlotId, SlotState, SlotVector};

use crate::error::{
    CALYX_SEXTANT_SLOT_ALREADY_REGISTERED, CALYX_SEXTANT_SLOT_INACTIVE, CALYX_SEXTANT_SLOT_MISSING,
    sextant_error,
};
use crate::index::{IndexSearchHit, IndexStats, SextantIndex};

type SharedIndex = Arc<RwLock<Box<dyn SextantIndex>>>;

#[derive(Clone)]
pub struct SlotIndexMap {
    panel_version: u32,
    declared_states: Arc<BTreeMap<SlotId, SlotState>>,
    indexes: Arc<RwLock<BTreeMap<SlotId, SharedIndex>>>,
    states: Arc<RwLock<BTreeMap<SlotId, SlotState>>>,
}

impl SlotIndexMap {
    pub fn new(panel: &Panel) -> Result<Self> {
        let mut declared_states = BTreeMap::new();
        for slot in &panel.slots {
            if declared_states.insert(slot.slot_id, slot.state).is_some() {
                return Err(sextant_error(
                    crate::error::CALYX_SEXTANT_PANEL_SCOPE_MISMATCH,
                    format!(
                        "panel {} declares local slot {} more than once",
                        panel.version, slot.slot_id
                    ),
                ));
            }
        }
        Ok(Self {
            panel_version: panel.version,
            declared_states: Arc::new(declared_states),
            indexes: Arc::new(RwLock::new(BTreeMap::new())),
            states: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    pub const fn panel_version(&self) -> u32 {
        self.panel_version
    }

    pub fn register<I>(&self, index: I) -> Result<()>
    where
        I: SextantIndex + 'static,
    {
        let slot = index.slot();
        let state = self.declared_states.get(&slot).copied().ok_or_else(|| {
            sextant_error(
                crate::error::CALYX_SEXTANT_PANEL_SCOPE_MISMATCH,
                format!(
                    "index slot {slot} is not declared by panel {}",
                    self.panel_version
                ),
            )
        })?;
        let mut indexes = self.indexes.write().expect("slot map poisoned");
        if indexes.contains_key(&slot) {
            return Err(sextant_error(
                CALYX_SEXTANT_SLOT_ALREADY_REGISTERED,
                format!("slot {slot} is already registered"),
            ));
        }
        indexes.insert(slot, Arc::new(RwLock::new(Box::new(index))));
        self.states
            .write()
            .expect("slot state map poisoned")
            .insert(slot, state);
        Ok(())
    }

    pub fn slots(&self) -> Vec<SlotId> {
        self.states
            .read()
            .expect("slot state map poisoned")
            .iter()
            .filter(|(_, state)| **state == SlotState::Active)
            .map(|(slot, _)| *slot)
            .collect()
    }

    pub fn registered_slots(&self) -> Vec<SlotId> {
        self.indexes
            .read()
            .expect("slot map poisoned")
            .keys()
            .copied()
            .collect()
    }

    pub fn set_slot_state(&self, slot: SlotId, state: SlotState) -> Result<()> {
        self.get(slot)?;
        self.states
            .write()
            .expect("slot state map poisoned")
            .insert(slot, state);
        Ok(())
    }

    pub fn slot_state(&self, slot: SlotId) -> Result<SlotState> {
        self.states
            .read()
            .expect("slot state map poisoned")
            .get(&slot)
            .copied()
            .ok_or_else(|| Self::missing_slot_error(slot))
    }

    pub fn stats(&self) -> Vec<IndexStats> {
        self.indexes
            .read()
            .expect("slot map poisoned")
            .values()
            .map(|index| index.read().expect("index poisoned").stats())
            .collect()
    }

    pub fn turboquant_prepared_count(&self, slot: SlotId) -> Result<usize> {
        let index = self.get(slot)?;
        Ok(index
            .read()
            .expect("index poisoned")
            .turboquant_prepared_count())
    }

    pub fn insert(&self, slot: SlotId, cx_id: CxId, vector: SlotVector, seq: u64) -> Result<()> {
        self.ensure_active(slot)?;
        let index = self.get(slot)?;
        index
            .write()
            .expect("index poisoned")
            .insert(cx_id, vector, seq)
    }

    pub fn insert_text(&self, slot: SlotId, cx_id: CxId, text: &str, seq: u64) -> Result<()> {
        self.ensure_active(slot)?;
        let index = self.get(slot)?;
        index
            .write()
            .expect("index poisoned")
            .insert_text(cx_id, text, seq)
    }

    pub fn search(
        &self,
        slot: SlotId,
        query: &SlotVector,
        k: usize,
        ef: Option<usize>,
    ) -> Result<Vec<IndexSearchHit>> {
        self.ensure_active(slot)?;
        let index = self.get(slot)?;
        index.read().expect("index poisoned").search(query, k, ef)
    }

    pub fn search_text(&self, slot: SlotId, text: &str, k: usize) -> Result<Vec<IndexSearchHit>> {
        self.ensure_active(slot)?;
        let index = self.get(slot)?;
        index.read().expect("index poisoned").search_text(text, k)
    }

    pub fn candidate_text(&self, slot: SlotId, cx_id: CxId) -> Result<Option<String>> {
        let index = self.get(slot)?;
        Ok(index.read().expect("index poisoned").candidate_text(cx_id))
    }

    pub fn vector(&self, slot: SlotId, cx_id: CxId) -> Result<Option<SlotVector>> {
        let index = self.get(slot)?;
        Ok(index.read().expect("index poisoned").vector(cx_id))
    }

    pub fn set_base_seq(&self, slot: SlotId, seq: u64) -> Result<()> {
        let index = self.get(slot)?;
        index.write().expect("index poisoned").set_base_seq(seq);
        Ok(())
    }

    pub fn rebuild(&self, slot: SlotId) -> Result<()> {
        self.ensure_active(slot)?;
        let index = self.get(slot)?;
        index.write().expect("index poisoned").rebuild()
    }

    pub fn missing_slot_error(slot: SlotId) -> calyx_core::CalyxError {
        sextant_error(
            CALYX_SEXTANT_SLOT_MISSING,
            format!("slot {slot} is not registered in the panel-bound index map"),
        )
    }

    pub fn assert_isolated(&self, a: SlotId, b: SlotId, query: &SlotVector) -> Result<bool> {
        let left: BTreeSet<_> = self
            .search(a, query, 5, None)?
            .into_iter()
            .map(|h| h.cx_id)
            .collect();
        let right: BTreeSet<_> = self
            .search(b, query, 5, None)?
            .into_iter()
            .map(|h| h.cx_id)
            .collect();
        Ok(left != right)
    }

    fn get(&self, slot: SlotId) -> Result<SharedIndex> {
        self.indexes
            .read()
            .expect("slot map poisoned")
            .get(&slot)
            .cloned()
            .ok_or_else(|| Self::missing_slot_error(slot))
    }

    fn ensure_active(&self, slot: SlotId) -> Result<()> {
        let state = self.slot_state(slot)?;
        if state == SlotState::Active {
            return Ok(());
        }
        Err(sextant_error(
            CALYX_SEXTANT_SLOT_INACTIVE,
            format!("slot {slot} is {state:?}"),
        ))
    }
}
