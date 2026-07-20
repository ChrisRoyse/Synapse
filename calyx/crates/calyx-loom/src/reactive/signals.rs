//! Real [`ReactiveSignals`] sources backing reactive conditions.

use std::collections::BTreeMap;
use std::sync::Mutex;

use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, CxId, PanelSlotId, Result, SlotId, SlotVector, VaultStore};
use calyx_ward::{GuardProfile, NoveltyAction, ProducedSlots, WardError};

use super::{NoveltyVerdict, ReactiveSignals};
use crate::cross_term::agreement_scalar;
use crate::error::{CALYX_REACTIVE_SIGNAL_UNAVAILABLE, loom_error};
use crate::recurrence::SeriesStore;

/// A [`ReactiveSignals`] source backed by the durable recurrence store. It
/// answers [`super::TriggerCondition::EventRecurs`] from the real, on-disk
/// occurrence count via [`SeriesStore::occurrence_count`].
///
/// Novelty and drift are outside this source's domain — it owns no Ward profile
/// and no agreement graph — so those methods **fail closed** with
/// [`CALYX_REACTIVE_SIGNAL_UNAVAILABLE`] rather than silently report "no match".
/// Compose a Ward-backed and an agreement-graph-backed source to cover
/// `NewRegion` and `DriftDetected`.
pub struct RecurrenceSignals<'a, C: Clock> {
    store: SeriesStore<'a, C>,
}

impl<'a, C: Clock> RecurrenceSignals<'a, C> {
    /// Wraps `vault`'s recurrence store as a reactive signal source.
    pub fn new(vault: &'a AsterVault<C>) -> Self {
        Self {
            store: SeriesStore::new(vault),
        }
    }
}

impl<C: Clock> ReactiveSignals for RecurrenceSignals<'_, C> {
    fn novelty(&self, _cx_id: CxId, _tau_override: Option<f32>) -> Result<NoveltyVerdict> {
        Err(loom_error(
            CALYX_REACTIVE_SIGNAL_UNAVAILABLE,
            "recurrence signal source cannot evaluate a NewRegion novelty verdict",
        ))
    }

    fn occurrence_count(&self, series: CxId) -> Result<u64> {
        self.store.occurrence_count(series)
    }

    fn slot_drift(&self, _slot: PanelSlotId) -> Result<f32> {
        Err(loom_error(
            CALYX_REACTIVE_SIGNAL_UNAVAILABLE,
            "recurrence signal source cannot evaluate a DriftDetected delta",
        ))
    }
}

/// Ward-backed novelty source for [`super::TriggerCondition::NewRegion`].
pub struct WardNoveltySignals<'a, C: Clock> {
    vault: &'a AsterVault<C>,
    profile: GuardProfile,
    matched_cx: CxId,
    high_stakes: bool,
}

impl<'a, C: Clock> WardNoveltySignals<'a, C> {
    pub fn new(
        vault: &'a AsterVault<C>,
        profile: GuardProfile,
        matched_cx: CxId,
        high_stakes: bool,
    ) -> Self {
        Self {
            vault,
            profile,
            matched_cx,
            high_stakes,
        }
    }

    fn verdict(&self, cx_id: CxId, tau_override: Option<f32>) -> Result<NoveltyVerdict> {
        let mut profile = self.profile.clone();
        if let Some(tau) = tau_override {
            for slot in &profile.required_slots {
                profile.tau.insert(*slot, tau);
            }
        }
        let produced = slots_for(
            self.vault,
            cx_id,
            profile.panel_version,
            &profile.required_slots,
        )?;
        let matched = slots_for(
            self.vault,
            self.matched_cx,
            profile.panel_version,
            &profile.required_slots,
        )?;
        let verdict = calyx_ward::guard(&profile, &produced, &matched, self.high_stakes)
            .map_err(ward_to_calyx)?;
        if verdict.action == Some(NoveltyAction::NewRegion) {
            Ok(NoveltyVerdict::NewRegion)
        } else {
            Ok(NoveltyVerdict::Grounded)
        }
    }
}

/// Tracks previous slot vectors so drift can be computed from real consecutive
/// vault snapshots.
#[derive(Debug, Default)]
pub struct AgreementDriftTracker {
    previous: Mutex<BTreeMap<PanelSlotId, Vec<f32>>>,
}

impl AgreementDriftTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn drift<C: Clock>(
        &self,
        vault: &AsterVault<C>,
        cx_id: CxId,
        panel_slot: PanelSlotId,
    ) -> Result<f32> {
        let current = dense_slot(vault, cx_id, panel_slot)?;
        let mut previous = self.previous.lock().map_err(|_| {
            loom_error(
                CALYX_REACTIVE_SIGNAL_UNAVAILABLE,
                "agreement drift tracker lock poisoned",
            )
        })?;
        let drift = previous
            .get(&panel_slot)
            .map(|prior| agreement_scalar(&current, prior).map(|cos| (1.0 - cos).abs()))
            .transpose()?
            .unwrap_or(0.0);
        previous.insert(panel_slot, current);
        Ok(drift)
    }
}

/// Agreement-cosine drift source for the current post-ingest constellation.
pub struct AgreementDriftSignals<'a, C: Clock> {
    vault: &'a AsterVault<C>,
    current_cx: CxId,
    tracker: &'a AgreementDriftTracker,
}

impl<'a, C: Clock> AgreementDriftSignals<'a, C> {
    pub fn new(
        vault: &'a AsterVault<C>,
        current_cx: CxId,
        tracker: &'a AgreementDriftTracker,
    ) -> Self {
        Self {
            vault,
            current_cx,
            tracker,
        }
    }
}

/// Composite real signal source. Recurrence is always available; Ward novelty
/// and agreement drift are opt-in because they need per-application context.
pub struct ReactiveSignalSet<'a, C: Clock> {
    recurrence: RecurrenceSignals<'a, C>,
    novelty: Option<WardNoveltySignals<'a, C>>,
    drift: Option<AgreementDriftSignals<'a, C>>,
}

impl<'a, C: Clock> ReactiveSignalSet<'a, C> {
    pub fn new(vault: &'a AsterVault<C>) -> Self {
        Self {
            recurrence: RecurrenceSignals::new(vault),
            novelty: None,
            drift: None,
        }
    }

    pub fn with_ward_novelty(
        mut self,
        profile: GuardProfile,
        matched_cx: CxId,
        high_stakes: bool,
    ) -> Self {
        let vault = self.recurrence.vault();
        self.novelty = Some(WardNoveltySignals::new(
            vault,
            profile,
            matched_cx,
            high_stakes,
        ));
        self
    }

    pub fn with_agreement_drift(
        mut self,
        current_cx: CxId,
        tracker: &'a AgreementDriftTracker,
    ) -> Self {
        let vault = self.recurrence.vault();
        self.drift = Some(AgreementDriftSignals::new(vault, current_cx, tracker));
        self
    }
}

impl<C: Clock> ReactiveSignals for WardNoveltySignals<'_, C> {
    fn novelty(&self, cx_id: CxId, tau_override: Option<f32>) -> Result<NoveltyVerdict> {
        self.verdict(cx_id, tau_override)
    }

    fn occurrence_count(&self, _series: CxId) -> Result<u64> {
        Err(unavailable(
            "Ward novelty source cannot evaluate EventRecurs",
        ))
    }

    fn slot_drift(&self, _slot: PanelSlotId) -> Result<f32> {
        Err(unavailable(
            "Ward novelty source cannot evaluate DriftDetected",
        ))
    }
}

impl<C: Clock> ReactiveSignals for AgreementDriftSignals<'_, C> {
    fn novelty(&self, _cx_id: CxId, _tau_override: Option<f32>) -> Result<NoveltyVerdict> {
        Err(unavailable(
            "agreement drift source cannot evaluate NewRegion",
        ))
    }

    fn occurrence_count(&self, _series: CxId) -> Result<u64> {
        Err(unavailable(
            "agreement drift source cannot evaluate EventRecurs",
        ))
    }

    fn slot_drift(&self, slot: PanelSlotId) -> Result<f32> {
        self.tracker.drift(self.vault, self.current_cx, slot)
    }
}

impl<C: Clock> ReactiveSignals for ReactiveSignalSet<'_, C> {
    fn novelty(&self, cx_id: CxId, tau_override: Option<f32>) -> Result<NoveltyVerdict> {
        self.novelty
            .as_ref()
            .ok_or_else(|| unavailable("composite reactive source has no Ward novelty adapter"))?
            .novelty(cx_id, tau_override)
    }

    fn occurrence_count(&self, series: CxId) -> Result<u64> {
        self.recurrence.occurrence_count(series)
    }

    fn slot_drift(&self, slot: PanelSlotId) -> Result<f32> {
        self.drift
            .as_ref()
            .ok_or_else(|| unavailable("composite reactive source has no agreement drift adapter"))?
            .slot_drift(slot)
    }
}

impl<'a, C: Clock> RecurrenceSignals<'a, C> {
    fn vault(&self) -> &'a AsterVault<C> {
        self.store.vault()
    }
}

fn slots_for<C: Clock>(
    vault: &AsterVault<C>,
    cx_id: CxId,
    panel_version: u32,
    required_slots: &[SlotId],
) -> Result<ProducedSlots> {
    let mut out = ProducedSlots::new();
    for slot in required_slots {
        out.insert(
            *slot,
            dense_slot(vault, cx_id, PanelSlotId::new(panel_version, *slot))?,
        );
    }
    Ok(out)
}

fn dense_slot<C: Clock>(
    vault: &AsterVault<C>,
    cx_id: CxId,
    panel_slot: PanelSlotId,
) -> Result<Vec<f32>> {
    let cx = vault.get(cx_id, vault.latest_seq())?;
    if cx.panel_version != panel_slot.panel_version() {
        return Err(loom_error(
            crate::error::CALYX_LOOM_PANEL_SCOPE_REQUIRED,
            format!(
                "constellation {cx_id} belongs to panel {}, not requested {panel_slot}",
                cx.panel_version
            ),
        ));
    }
    let vector = cx
        .slots
        .get(&panel_slot.slot_id())
        .ok_or_else(|| unavailable(format!("missing dense {panel_slot} for {cx_id}")))?;
    match vector {
        SlotVector::Dense { data, .. } => Ok(data.clone()),
        other => Err(unavailable(format!(
            "{panel_slot} for {cx_id} is not dense: {other:?}"
        ))),
    }
}

fn ward_to_calyx(error: WardError) -> CalyxError {
    CalyxError {
        code: error.code(),
        message: error.to_string(),
        remediation: "repair the Ward guard profile or slot rows before reactive evaluation",
    }
}

fn unavailable(message: impl Into<String>) -> CalyxError {
    loom_error(CALYX_REACTIVE_SIGNAL_UNAVAILABLE, message)
}
