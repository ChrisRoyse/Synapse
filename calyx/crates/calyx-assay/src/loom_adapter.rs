//! Aster-backed Assay adapter for Loom materialization planning.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use calyx_core::{
    Anchor, AnchorKind, AnchorValue, CalyxError, CxId, Panel, PanelSlotId, Result, Seq, SlotId,
    SlotState, SlotVector, VaultStore,
};
use calyx_loom::{MaterializationPlan, plan_cross_terms_checked};

use crate::estimate::require_grounded_anchor;
use crate::gate::{AssayGate, PairGain, pair_gain_from_estimates};

type PairSamples = (Vec<Vec<f32>>, Vec<Vec<f32>>, Vec<bool>);

pub struct AsterAssayMaterializationGate<'a, S: VaultStore + ?Sized> {
    store: &'a S,
    snapshot: Seq,
    cx_ids: Vec<CxId>,
    panel_version: u32,
    active_slots: BTreeSet<SlotId>,
    anchor_kind: AnchorKind,
    assay: AssayGate,
    last_error: Mutex<Option<CalyxError>>,
    error_count: AtomicU64,
}

impl<'a, S> AsterAssayMaterializationGate<'a, S>
where
    S: VaultStore + ?Sized,
{
    pub fn new(store: &'a S, panel: &Panel, cx_ids: Vec<CxId>, anchor_kind: AnchorKind) -> Self {
        Self::at_snapshot(store, store.snapshot(), panel, cx_ids, anchor_kind)
    }

    pub fn at_snapshot(
        store: &'a S,
        snapshot: Seq,
        panel: &Panel,
        cx_ids: Vec<CxId>,
        anchor_kind: AnchorKind,
    ) -> Self {
        Self {
            store,
            snapshot,
            cx_ids,
            panel_version: panel.version,
            active_slots: panel
                .slots
                .iter()
                .filter(|slot| slot.state == SlotState::Active)
                .map(|slot| slot.slot_id)
                .collect(),
            anchor_kind,
            assay: AssayGate::default(),
            last_error: Mutex::new(None),
            error_count: AtomicU64::new(0),
        }
    }

    pub fn with_min_samples(mut self, min_samples: usize) -> Self {
        self.assay.min_samples = min_samples;
        self
    }

    pub fn pair_gain(&self, a: PanelSlotId, b: PanelSlotId) -> Result<PairGain> {
        let a = self.local_slot(a)?;
        let b = self.local_slot(b)?;
        let (left, right, labels) = self.load_pair_samples(a, b)?;
        self.assay.pair_gain(&left, &right, &labels)
    }

    pub fn materialization_plan(&self, slots: &[PanelSlotId]) -> Result<MaterializationPlan> {
        self.materialization_plan_cached(slots)
            .inspect_err(|error| self.record_error(error.clone()))
    }

    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
    }

    fn materialization_plan_cached(
        &self,
        panel_slots: &[PanelSlotId],
    ) -> Result<MaterializationPlan> {
        let slots = panel_slots
            .iter()
            .copied()
            .map(|slot| self.local_slot(slot))
            .collect::<Result<Vec<_>>>()?;
        let samples = self.load_slot_samples(&slots)?;
        let solo = samples
            .slots
            .iter()
            .map(|(slot, values)| {
                self.assay
                    .lens_signal(values, &samples.labels)
                    .map(|signal| (*slot, signal.estimate))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        plan_cross_terms_checked(&slots, |a, b| {
            let combined = combine_samples(&samples.slots[&a], &samples.slots[&b]);
            let pair = self.assay.lens_signal(&combined, &samples.labels)?.estimate;
            Ok(pair_gain_from_estimates(&solo[&a], &solo[&b], &pair)?.gain_bits)
        })
    }

    fn load_slot_samples(&self, slots: &[SlotId]) -> Result<SlotSampleSet> {
        let mut values = slots
            .iter()
            .copied()
            .map(|slot| (slot, Vec::with_capacity(self.cx_ids.len())))
            .collect::<BTreeMap<_, _>>();
        let mut labels = Vec::with_capacity(self.cx_ids.len());
        for cx_id in &self.cx_ids {
            let cx = self.store.get(*cx_id, self.snapshot)?;
            if cx.panel_version != self.panel_version {
                return Err(CalyxError::stale_derived(format!(
                    "assay panel {} rejected constellation {cx_id} from panel {}",
                    self.panel_version, cx.panel_version
                )));
            }
            for slot in slots {
                let vector = cx.slots.get(slot).ok_or_else(|| {
                    CalyxError::stale_derived(format!("slot {} missing for {cx_id}", slot.get()))
                })?;
                values
                    .get_mut(slot)
                    .expect("slot samples initialized")
                    .push(features(vector)?);
            }
            let anchor = cx
                .anchors
                .iter()
                .find(|anchor| anchor.kind == self.anchor_kind)
                .ok_or_else(|| {
                    CalyxError::stale_derived(format!(
                        "anchor {:?} missing for {cx_id}",
                        self.anchor_kind
                    ))
                })?;
            labels.push(anchor_bool(anchor)?);
        }
        Ok(SlotSampleSet {
            slots: values,
            labels,
        })
    }
    pub fn last_error(&self) -> Option<CalyxError> {
        self.last_error
            .lock()
            .expect("materialization gate error mutex poisoned")
            .clone()
    }

    fn record_error(&self, error: CalyxError) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .expect("materialization gate error mutex poisoned") = Some(error);
    }

    fn load_pair_samples(&self, a: SlotId, b: SlotId) -> Result<PairSamples> {
        let mut left = Vec::with_capacity(self.cx_ids.len());
        let mut right = Vec::with_capacity(self.cx_ids.len());
        let mut labels = Vec::with_capacity(self.cx_ids.len());
        for cx_id in &self.cx_ids {
            let cx = self.store.get(*cx_id, self.snapshot)?;
            if cx.panel_version != self.panel_version {
                return Err(CalyxError::stale_derived(format!(
                    "assay panel {} rejected constellation {cx_id} from panel {}",
                    self.panel_version, cx.panel_version
                )));
            }
            let left_vector = cx.slots.get(&a).ok_or_else(|| {
                CalyxError::stale_derived(format!("slot {} missing for {cx_id}", a.get()))
            })?;
            let right_vector = cx.slots.get(&b).ok_or_else(|| {
                CalyxError::stale_derived(format!("slot {} missing for {cx_id}", b.get()))
            })?;
            let anchor = cx
                .anchors
                .iter()
                .find(|anchor| anchor.kind == self.anchor_kind)
                .ok_or_else(|| {
                    CalyxError::stale_derived(format!(
                        "anchor {:?} missing for {cx_id}",
                        self.anchor_kind
                    ))
                })?;
            left.push(features(left_vector)?);
            right.push(features(right_vector)?);
            labels.push(anchor_bool(anchor)?);
        }
        Ok((left, right, labels))
    }

    fn local_slot(&self, panel_slot: PanelSlotId) -> Result<SlotId> {
        if panel_slot.panel_version() != self.panel_version
            || !self.active_slots.contains(&panel_slot.slot_id())
        {
            return Err(CalyxError::stale_derived(format!(
                "assay gate for panel {} rejected inactive or mismatched {panel_slot}",
                self.panel_version
            )));
        }
        Ok(panel_slot.slot_id())
    }
}

struct SlotSampleSet {
    slots: BTreeMap<SlotId, Vec<Vec<f32>>>,
    labels: Vec<bool>,
}

fn combine_samples(left: &[Vec<f32>], right: &[Vec<f32>]) -> Vec<Vec<f32>> {
    left.iter()
        .zip(right)
        .map(|(a, b)| a.iter().chain(b).copied().collect())
        .collect()
}

fn features(vector: &SlotVector) -> Result<Vec<f32>> {
    match vector {
        SlotVector::Dense { data, .. } => Ok(data.clone()),
        SlotVector::Sparse { dim, entries } => {
            let mut dense = vec![0.0; *dim as usize];
            for entry in entries {
                let index = entry.idx as usize;
                if index >= dense.len() {
                    return Err(CalyxError::aster_corrupt_shard(
                        "sparse slot entry exceeds declared dimension",
                    ));
                }
                dense[index] = entry.val;
            }
            Ok(dense)
        }
        SlotVector::Multi { .. } | SlotVector::Absent { .. } => Err(CalyxError::stale_derived(
            "materialization gate requires dense or sparse measured slots",
        )),
    }
}

fn anchor_bool(anchor: &Anchor) -> Result<bool> {
    require_grounded_anchor(anchor)?;
    match &anchor.value {
        AnchorValue::Bool(value) => Ok(*value),
        AnchorValue::Number(value) if value.is_finite() => Ok(*value > 0.0),
        _ => Err(CalyxError::assay_insufficient_samples(
            "materialization gate requires binary anchor values",
        )),
    }
}
