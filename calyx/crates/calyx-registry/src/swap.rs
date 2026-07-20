use std::collections::BTreeMap;

use calyx_core::{
    Asymmetry, CalyxError, CxId, LensId, Modality, Panel, PanelSlotId, QuantPolicy, Result, Slot,
    SlotId, SlotKey, SlotShape, SlotState, Ts,
};
use serde::{Deserialize, Serialize};

use crate::backfill::{BackfillPriority, BackfillRequest, BackfillScheduler};
use crate::lens::Registry;

/// Slot declaration supplied when a lens is hot-added to a panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotSpec {
    pub key: String,
    pub lens_id: LensId,
    pub shape: SlotShape,
    pub modality: Modality,
    pub asymmetry: Asymmetry,
    pub quant: QuantPolicy,
    pub axis: Option<String>,
    #[serde(default)]
    pub retrieval_only: bool,
    #[serde(default)]
    pub excluded_from_dedup: bool,
}

impl SlotSpec {
    pub fn dense_text(key: impl Into<String>, lens_id: LensId, dim: u32) -> Self {
        Self {
            key: key.into(),
            lens_id,
            shape: SlotShape::Dense(dim),
            modality: Modality::Text,
            asymmetry: Asymmetry::None,
            quant: QuantPolicy::None,
            axis: None,
            retrieval_only: false,
            excluded_from_dedup: false,
        }
    }

    pub const fn with_usage_flags(
        mut self,
        retrieval_only: bool,
        excluded_from_dedup: bool,
    ) -> Self {
        self.retrieval_only = retrieval_only;
        self.excluded_from_dedup = excluded_from_dedup;
        self
    }
}

/// A constellation scheduled for lazy backfill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillCandidate {
    pub cx_id: CxId,
    pub priority: u32,
}

/// Priority-ordered, resumable backfill queue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillQueue {
    next_seq: u64,
    tasks: BTreeMap<BackfillTaskId, BackfillTask>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BackfillTaskId(u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillTask {
    pub id: BackfillTaskId,
    pub panel_slot: PanelSlotId,
    pub lens_id: LensId,
    pub cx_id: CxId,
    pub priority: u32,
    pub attempts: u16,
    pub state: BackfillState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillState {
    Pending,
    InFlight,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPlaceholder {
    pub slot_id: SlotId,
    pub lens_id: LensId,
    pub ready: bool,
    pub queued: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddLensOutcome {
    pub slot: Slot,
    pub panel_version: u32,
    pub index: IndexPlaceholder,
    pub queued: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleOutcome {
    pub slot_id: SlotId,
    pub lens_id: LensId,
    pub state: SlotState,
    pub panel_version: u32,
}

/// Mutable lifecycle controller for one panel plus its lazy backfill queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SwapController {
    panel: Panel,
    queue: BackfillQueue,
}

impl SwapController {
    pub fn new(panel: Panel) -> Self {
        Self {
            panel,
            queue: BackfillQueue::default(),
        }
    }

    pub const fn panel(&self) -> &Panel {
        &self.panel
    }

    pub const fn queue(&self) -> &BackfillQueue {
        &self.queue
    }

    pub fn queue_mut(&mut self) -> &mut BackfillQueue {
        &mut self.queue
    }

    pub fn add_lens<I>(
        &mut self,
        registry: &Registry,
        spec: SlotSpec,
        candidates: I,
        now: Ts,
    ) -> Result<AddLensOutcome>
    where
        I: IntoIterator<Item = BackfillCandidate>,
    {
        if let Some(slot) = identical_live_slot(&self.panel, &spec) {
            ensure_registered_lens(registry, &spec)?;
            return Ok(AddLensOutcome {
                slot: slot.clone(),
                panel_version: self.panel.version,
                index: IndexPlaceholder {
                    slot_id: slot.slot_id,
                    lens_id: slot.lens_id,
                    ready: true,
                    queued: 0,
                },
                queued: 0,
            });
        }
        ensure_unique_slot(&self.panel, &spec)?;
        ensure_registered_lens(registry, &spec)?;
        let slot_id = next_slot_id(&self.panel)?;
        let version = self.bump_panel(now)?;
        let slot = Slot {
            slot_id,
            slot_key: SlotKey::new(slot_id, spec.key),
            lens_id: spec.lens_id,
            shape: spec.shape,
            modality: spec.modality,
            asymmetry: spec.asymmetry,
            quant: spec.quant,
            resource: Default::default(),
            axis: spec.axis,
            retrieval_only: spec.retrieval_only,
            excluded_from_dedup: spec.excluded_from_dedup,
            bits_about: BTreeMap::new(),
            state: SlotState::Active,
            added_at_panel_version: version,
        };
        self.panel.slots.push(slot.clone());
        let queued = self.queue.enqueue_many(
            PanelSlotId::new(version, slot.slot_id),
            slot.lens_id,
            candidates,
        );
        Ok(AddLensOutcome {
            slot,
            panel_version: version,
            index: IndexPlaceholder {
                slot_id,
                lens_id: spec.lens_id,
                ready: false,
                queued,
            },
            queued,
        })
    }

    pub fn add_lens_durable<I>(
        &mut self,
        registry: &Registry,
        spec: SlotSpec,
        candidates: I,
        now: Ts,
        scheduler: &mut BackfillScheduler,
        priority: BackfillPriority,
    ) -> Result<AddLensOutcome>
    where
        I: IntoIterator<Item = BackfillCandidate>,
    {
        let candidates = candidates.into_iter().collect::<Vec<_>>();
        let panel_before = self.panel.clone();
        let queue_before = self.queue.clone();
        let scheduler_before = scheduler.clone();
        let outcome = self.add_lens(registry, spec, candidates.iter().copied(), now)?;
        if outcome.index.ready && outcome.queued == 0 {
            return Ok(outcome);
        }
        let request = BackfillRequest {
            panel_slot: PanelSlotId::new(outcome.panel_version, outcome.slot.slot_id),
            lens_id: outcome.slot.lens_id,
            priority,
            candidates: candidates.iter().map(|candidate| candidate.cx_id).collect(),
        };
        if let Err(error) = scheduler.enqueue(request) {
            self.panel = panel_before;
            self.queue = queue_before;
            *scheduler = scheduler_before;
            return Err(error);
        }
        Ok(outcome)
    }

    pub fn park_lens(&mut self, slot_id: SlotId, now: Ts) -> Result<LifecycleOutcome> {
        self.set_slot_state(slot_id, SlotState::Parked, now)
    }

    pub fn unpark_lens(&mut self, slot_id: SlotId, now: Ts) -> Result<LifecycleOutcome> {
        self.set_slot_state(slot_id, SlotState::Active, now)
    }

    pub fn retire_lens(&mut self, slot_id: SlotId, now: Ts) -> Result<LifecycleOutcome> {
        self.set_slot_state(slot_id, SlotState::Retired, now)
    }

    fn set_slot_state(
        &mut self,
        slot_id: SlotId,
        state: SlotState,
        now: Ts,
    ) -> Result<LifecycleOutcome> {
        let index = self.slot_index(slot_id)?;
        let current = self.panel.slots[index].state;
        if current == state {
            return Ok(LifecycleOutcome {
                slot_id,
                lens_id: self.panel.slots[index].lens_id,
                state,
                panel_version: self.panel.version,
            });
        }
        if current == SlotState::Retired {
            return Err(CalyxError::lens_frozen_violation(format!(
                "slot {slot_id} is retired and cannot transition to {state:?}"
            )));
        }
        let prior_version = self.panel.version;
        let version = self.bump_panel(now)?;
        self.panel.slots[index].state = state;
        if state != SlotState::Active {
            self.queue
                .cancel_slot(PanelSlotId::new(prior_version, slot_id));
        }
        Ok(LifecycleOutcome {
            slot_id,
            lens_id: self.panel.slots[index].lens_id,
            state,
            panel_version: version,
        })
    }

    fn slot_index(&self, slot_id: SlotId) -> Result<usize> {
        self.panel
            .slots
            .iter()
            .position(|slot| slot.slot_id == slot_id)
            .ok_or_else(|| {
                CalyxError::lens_frozen_violation(format!("slot {slot_id} is not in panel"))
            })
    }

    fn bump_panel(&mut self, now: Ts) -> Result<u32> {
        self.panel.version = self
            .panel
            .version
            .checked_add(1)
            .ok_or_else(|| CalyxError::lens_frozen_violation("panel version overflow"))?;
        self.panel.created_at = now;
        Ok(self.panel.version)
    }
}

impl BackfillQueue {
    pub fn enqueue_many<I>(
        &mut self,
        panel_slot: PanelSlotId,
        lens_id: LensId,
        candidates: I,
    ) -> usize
    where
        I: IntoIterator<Item = BackfillCandidate>,
    {
        let mut count = 0;
        for candidate in candidates {
            self.enqueue(panel_slot, lens_id, candidate);
            count += 1;
        }
        count
    }

    pub fn enqueue(
        &mut self,
        panel_slot: PanelSlotId,
        lens_id: LensId,
        candidate: BackfillCandidate,
    ) -> BackfillTaskId {
        let id = BackfillTaskId(self.next_seq);
        self.next_seq += 1;
        self.tasks.insert(
            id,
            BackfillTask {
                id,
                panel_slot,
                lens_id,
                cx_id: candidate.cx_id,
                priority: candidate.priority,
                attempts: 0,
                state: BackfillState::Pending,
            },
        );
        id
    }

    pub fn claim_batch(&mut self, limit: usize) -> Vec<BackfillTask> {
        let mut ids = self.pending_ids();
        ids.truncate(limit);
        ids.into_iter()
            .filter_map(|id| {
                let task = self.tasks.get_mut(&id)?;
                task.state = BackfillState::InFlight;
                Some(task.clone())
            })
            .collect()
    }

    pub fn complete(&mut self, id: BackfillTaskId) -> Result<()> {
        let task = self.task_mut(id)?;
        task.state = BackfillState::Complete;
        Ok(())
    }

    pub fn retry(&mut self, id: BackfillTaskId) -> Result<()> {
        let task = self.task_mut(id)?;
        task.state = BackfillState::Pending;
        task.attempts = task.attempts.saturating_add(1);
        Ok(())
    }

    pub fn pending_len(&self) -> usize {
        self.count_state(BackfillState::Pending)
    }

    pub fn completed_len(&self) -> usize {
        self.count_state(BackfillState::Complete)
    }

    pub fn cancel_slot(&mut self, panel_slot: PanelSlotId) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|_, task| {
            task.panel_slot != panel_slot || task.state == BackfillState::Complete
        });
        before - self.tasks.len()
    }

    pub fn tasks(&self) -> impl Iterator<Item = &BackfillTask> {
        self.tasks.values()
    }

    fn pending_ids(&self) -> Vec<BackfillTaskId> {
        let mut tasks = self
            .tasks
            .values()
            .filter(|task| task.state == BackfillState::Pending)
            .collect::<Vec<_>>();
        tasks.sort_by_key(|task| (std::cmp::Reverse(task.priority), task.id));
        tasks.into_iter().map(|task| task.id).collect()
    }

    fn count_state(&self, state: BackfillState) -> usize {
        self.tasks
            .values()
            .filter(|task| task.state == state)
            .count()
    }

    fn task_mut(&mut self, id: BackfillTaskId) -> Result<&mut BackfillTask> {
        self.tasks
            .get_mut(&id)
            .ok_or_else(|| CalyxError::stale_derived(format!("backfill task {} is missing", id.0)))
    }
}

fn identical_live_slot<'a>(panel: &'a Panel, spec: &SlotSpec) -> Option<&'a Slot> {
    panel.slots.iter().find(|slot| {
        slot.state != SlotState::Retired
            && slot.slot_key.key() == spec.key
            && slot.lens_id == spec.lens_id
            && slot.shape == spec.shape
            && slot.modality == spec.modality
            && slot.asymmetry == spec.asymmetry
            && slot.quant == spec.quant
            && slot.axis == spec.axis
            && slot.retrieval_only == spec.retrieval_only
            && slot.excluded_from_dedup == spec.excluded_from_dedup
    })
}

fn ensure_unique_slot(panel: &Panel, spec: &SlotSpec) -> Result<()> {
    if panel
        .slots
        .iter()
        .any(|slot| slot.slot_key.key() == spec.key)
    {
        return Err(CalyxError::lens_frozen_violation(format!(
            "slot key {} already exists",
            spec.key
        )));
    }
    if panel
        .slots
        .iter()
        .any(|slot| slot.lens_id == spec.lens_id && slot.state != SlotState::Retired)
    {
        return Err(CalyxError::lens_frozen_violation(format!(
            "lens {} is already active or parked",
            spec.lens_id
        )));
    }
    Ok(())
}

fn ensure_registered_lens(registry: &Registry, spec: &SlotSpec) -> Result<()> {
    let contract = registry.frozen_contract(spec.lens_id).ok_or_else(|| {
        CalyxError::lens_frozen_violation(format!(
            "lens {} is not registered with a frozen contract",
            spec.lens_id
        ))
    })?;
    if contract.shape() != spec.shape {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "slot {} shape {:?} != frozen {:?}",
            spec.key,
            spec.shape,
            contract.shape()
        )));
    }
    if contract.modality() != spec.modality {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "slot {} modality {:?} != frozen {:?}",
            spec.key,
            spec.modality,
            contract.modality()
        )));
    }
    Ok(())
}

fn next_slot_id(panel: &Panel) -> Result<SlotId> {
    let next = panel
        .slots
        .iter()
        .map(|slot| slot.slot_id.get())
        .max()
        .map_or(0, |id| id.saturating_add(1));
    if next == u16::MAX && panel.slots.iter().any(|slot| slot.slot_id.get() == next) {
        return Err(CalyxError::lens_frozen_violation("slot id overflow"));
    }
    Ok(SlotId::new(next))
}
