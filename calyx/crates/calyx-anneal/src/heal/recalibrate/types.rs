use calyx_aster::cf::full_content_hash;
use calyx_core::{CalyxError, LensId, PanelSlotId, Result};
use serde::{Deserialize, Serialize};

use crate::{
    ActionMetricSnapshot, AnnealAction, AnnealLedgerAction, ArtifactKey, ArtifactPtr, BudgetHandle,
    ChangeId, LogicalTime, MvccSnapshot, ReplayQuery, ShadowRevertReason,
};

pub const WARD_TAU_TAG: &str = "ward_tau_v2";
pub const SIGNAL_DECAY_FLOOR_BITS: f64 = 0.05;
pub const CALYX_ANNEAL_PARK_THRESHOLD_NOT_MET: &str = "CALYX_ANNEAL_PARK_THRESHOLD_NOT_MET";
pub const CALYX_ANNEAL_UNPARK_THRESHOLD_NOT_MET: &str = "CALYX_ANNEAL_UNPARK_THRESHOLD_NOT_MET";
pub const CALYX_ANNEAL_TAU_INVALID: &str = "CALYX_ANNEAL_TAU_INVALID";
pub const CALYX_WARD_RECALIBRATE_FAILED: &str = "CALYX_WARD_RECALIBRATE_FAILED";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewTau {
    pub panel_slot: PanelSlotId,
    pub tau: f32,
    pub far: f64,
    pub frr: f64,
    pub shadow_metrics: ActionMetricSnapshot,
}

impl NewTau {
    pub fn new(
        panel_slot: PanelSlotId,
        tau: f32,
        far: f64,
        frr: f64,
        shadow_metrics: ActionMetricSnapshot,
    ) -> Result<Self> {
        validate_tau(tau)?;
        validate_unit("far", far)?;
        validate_unit("frr", frr)?;
        Ok(Self {
            panel_slot,
            tau,
            far,
            frr,
            shadow_metrics,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TauDriftEvent {
    pub panel_slot: PanelSlotId,
    pub current_tau: f32,
    pub observed_far: f64,
    pub drift_tolerance: f64,
    pub observed_at: LogicalTime,
    pub snapshot: MvccSnapshot,
    pub incumbent_metrics: ActionMetricSnapshot,
}

impl TauDriftEvent {
    pub fn new(
        panel_slot: PanelSlotId,
        current_tau: f32,
        observed_far: f64,
        drift_tolerance: f64,
        observed_at: LogicalTime,
        snapshot: MvccSnapshot,
        incumbent_metrics: ActionMetricSnapshot,
    ) -> Result<Self> {
        validate_tau(current_tau)?;
        if !observed_far.is_finite() || observed_far < 0.0 {
            return Err(invalid_tau("observed_far must be finite and non-negative"));
        }
        if !drift_tolerance.is_finite() || drift_tolerance < 0.0 {
            return Err(invalid_tau(
                "drift_tolerance must be finite and non-negative",
            ));
        }
        Ok(Self {
            panel_slot,
            current_tau,
            observed_far,
            drift_tolerance,
            observed_at,
            snapshot,
            incumbent_metrics,
        })
    }
}

pub trait WardRecalibrate: Send + Sync {
    fn recalibrate(
        &self,
        panel_slot: PanelSlotId,
        snapshot: MvccSnapshot,
        budget: BudgetHandle,
    ) -> Result<NewTau>;
}

pub trait WardTauStore {
    fn current_tau(&self, panel_slot: PanelSlotId) -> Result<Option<f32>>;
    fn set_live_tau(
        &mut self,
        panel_slot: PanelSlotId,
        tau: &NewTau,
        updated_at: LogicalTime,
    ) -> Result<()>;
    fn readback(&self) -> Result<Vec<WardTauReadback>>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WardTauReadback {
    pub panel_slot: PanelSlotId,
    pub tau: f32,
    pub far: f64,
    pub frr: f64,
    pub updated_at: LogicalTime,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RecalibrationOutcome {
    Promoted {
        change_id: ChangeId,
        panel_slot: PanelSlotId,
        prior_tau: f32,
        new_tau: f32,
    },
    Reverted {
        change_id: ChangeId,
        panel_slot: PanelSlotId,
        prior_tau: f32,
        candidate_tau: f32,
        reason: ShadowRevertReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LensParkOutcome {
    Parked { lens_id: LensId },
    AlreadyParked { lens_id: LensId },
    Unparked { lens_id: LensId },
    AlreadyOk { lens_id: LensId },
}

#[derive(Clone)]
pub(super) struct TauShadowAction(pub(super) ActionMetricSnapshot);

impl AnnealAction for TauShadowAction {
    fn apply_shadow(&self, _query: &ReplayQuery) -> Result<ActionMetricSnapshot> {
        Ok(self.0.clone())
    }
}

pub(super) fn tau_artifact_key(panel_slot: PanelSlotId) -> ArtifactKey {
    ArtifactKey::ConfigCache(tau_hash(panel_slot, 0.0))
}

pub(super) fn tau_ptr(panel_slot: PanelSlotId, tau: f32) -> ArtifactPtr {
    ArtifactPtr::ConfigCacheKeyHash(tau_hash(panel_slot, tau))
}

pub(super) fn tau_hash(panel_slot: PanelSlotId, tau: f32) -> [u8; 32] {
    full_content_hash([
        b"guard_tau_v2".as_slice(),
        &panel_slot.panel_version().to_be_bytes(),
        &panel_slot.slot_id().get().to_be_bytes(),
        &tau.to_le_bytes(),
    ])
}

pub(super) fn ptr_hash(ptr: &ArtifactPtr) -> [u8; 32] {
    match ptr {
        ArtifactPtr::ConfigCacheKeyHash(hash) | ArtifactPtr::QuantLevelRecordHash(hash) => *hash,
        ArtifactPtr::HnswGraphPath(path) => full_content_hash([path.as_bytes()]),
    }
}

pub(super) fn lens_hash(lens_id: LensId, bits: f64, label: &[u8]) -> [u8; 32] {
    full_content_hash([lens_id.to_string().as_bytes(), &bits.to_le_bytes(), label])
}

pub(super) fn tau_change_id(
    panel_slot: PanelSlotId,
    ts: LogicalTime,
    action: AnnealLedgerAction,
) -> ChangeId {
    let hash = full_content_hash([
        b"tau-change".as_slice(),
        &panel_slot.panel_version().to_be_bytes(),
        &panel_slot.slot_id().get().to_be_bytes(),
        &ts.to_be_bytes(),
        action_label(action).as_bytes(),
    ]);
    change_id_from_hash(hash)
}

pub(super) fn lens_change_id(
    lens_id: LensId,
    ts: LogicalTime,
    action: AnnealLedgerAction,
) -> ChangeId {
    let hash = full_content_hash([
        b"lens-park".as_slice(),
        lens_id.to_string().as_bytes(),
        &ts.to_be_bytes(),
        action_label(action).as_bytes(),
    ]);
    change_id_from_hash(hash)
}

pub(super) fn action_label(action: AnnealLedgerAction) -> &'static str {
    match action {
        AnnealLedgerAction::TauRecalibrated => "tau_recalibrated",
        AnnealLedgerAction::TauRecalibrationReverted => "tau_recalibration_reverted",
        AnnealLedgerAction::LensPark => "lens_park",
        AnnealLedgerAction::LensUnpark => "lens_unpark",
        _ => "anneal_event",
    }
}

pub(super) fn validate_tau(tau: f32) -> Result<()> {
    if tau.is_finite() && (-1.0..=1.0).contains(&tau) {
        Ok(())
    } else {
        Err(invalid_tau("tau must be finite in -1.0..=1.0"))
    }
}

pub(super) fn invalid_tau(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_TAU_INVALID,
        message: message.into(),
        remediation: "repair the Ward tau calibration input before promoting",
    }
}

pub(super) fn threshold_not_met(code: &'static str, bits: f64, reason: &'static str) -> CalyxError {
    CalyxError {
        code,
        message: format!("{reason}; got bits={bits}"),
        remediation: "read Assay bits_per_anchor and retry only when the threshold is crossed",
    }
}

pub(super) fn ward_failed(error: CalyxError) -> CalyxError {
    CalyxError {
        code: CALYX_WARD_RECALIBRATE_FAILED,
        message: format!(
            "Ward recalibration failed: {}: {}",
            error.code, error.message
        ),
        remediation: "keep the incumbent tau and inspect Ward calibration inputs",
    }
}

pub(super) fn alert_error(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: crate::CALYX_ANNEAL_ALERT_WRITE_FAILED,
        message: message.into(),
        remediation: "repair the vault alert path; ledger event was already attempted",
    }
}

fn validate_unit(field: &'static str, value: f64) -> Result<()> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(invalid_tau(format!("{field} must be finite in 0.0..=1.0")))
    }
}

fn change_id_from_hash(hash: [u8; 32]) -> ChangeId {
    let mut raw = [0_u8; 8];
    raw.copy_from_slice(&hash[..8]);
    ChangeId(u64::from_be_bytes(raw).max(1))
}
