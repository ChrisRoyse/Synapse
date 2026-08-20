//! Guard profile configuration shared by Ward guard calls.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use calyx_core::{Clock, GuardTauProfile, SlotId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::calibrate::SlotKind;

/// Stable identifier for a guard profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GuardId(Uuid);

impl GuardId {
    /// Builds a guard id from a UUID.
    pub const fn new(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the wrapped UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for GuardId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

impl fmt::Display for GuardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for GuardId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Required-slot pass policy for a guard profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuardPolicy {
    /// Every required slot must pass its per-slot tau.
    AllRequired,
    /// At least `k` required slots must pass.
    KofN { k: usize },
}

/// Action to take when an input lands outside the calibrated region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoveltyAction {
    NewRegion,
    Quarantine,
    RejectClosed,
}

/// Calibration provenance attached to a guard profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationMeta {
    pub corpus_hash: [u8; 32],
    pub estimator: String,
    pub far: f32,
    pub frr: f32,
    pub confidence: f32,
    pub ts: i64,
    /// Maximum absolute score deviation covered by calibration when the
    /// calibration and serving reductions execute through different valid
    /// floating-point orders/backends. `None` identifies a legacy profile
    /// whose threshold has no serving-parity contract and is therefore not
    /// admissible for high-stakes use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_tolerance: Option<f32>,
    /// Versioned numeric score implementation used to construct the
    /// calibration corpus. Ward serving rejects a profile unless this names
    /// the exact scorer it executes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring_engine: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_slot: BTreeMap<SlotId, SlotCalibrationMeta>,
}

/// Per-slot calibration bounds preserved under a profile-level summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotCalibrationMeta {
    pub corpus_hash: [u8; 32],
    pub estimator: String,
    pub far: f32,
    pub frr: f32,
    pub confidence: f32,
    pub ts: i64,
    /// Per-slot serving-score deviation already folded conservatively into
    /// the calibrated score distribution. Absent on legacy profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_tolerance: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring_engine: Option<String>,
    /// The slot's aspect (Identity/Content/Stylistic), persisted at calibration
    /// time so the guard surface can label `perSlot.aspect` and report conformal
    /// FAR per aspect class (#1899). `None` for profiles calibrated before this
    /// field existed — surfaced honestly as a null aspect, never defaulted.
    #[serde(default)]
    pub slot_kind: Option<SlotKind>,
}

impl CalibrationMeta {
    /// Builds calibration metadata from an injected Calyx clock.
    pub fn new(
        corpus_hash: [u8; 32],
        estimator: impl Into<String>,
        far: f32,
        frr: f32,
        confidence: f32,
        clock: &dyn Clock,
    ) -> Self {
        Self {
            corpus_hash,
            estimator: estimator.into(),
            far,
            frr,
            confidence,
            ts: clock_ts_i64(clock),
            score_tolerance: None,
            scoring_engine: None,
            per_slot: BTreeMap::new(),
        }
    }
}

impl SlotCalibrationMeta {
    pub fn from_calibration(meta: &CalibrationMeta, slot_kind: SlotKind) -> Self {
        Self {
            corpus_hash: meta.corpus_hash,
            estimator: meta.estimator.clone(),
            far: meta.far,
            frr: meta.frr,
            confidence: meta.confidence,
            ts: meta.ts,
            score_tolerance: meta.score_tolerance,
            scoring_engine: meta.scoring_engine.clone(),
            slot_kind: Some(slot_kind),
        }
    }
}

fn clock_ts_i64(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now()).unwrap_or(i64::MAX)
}

/// Configuration object read by Ward guard calls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuardProfile {
    pub guard_id: GuardId,
    pub panel_version: u32,
    pub domain: String,
    /// Exact grounded outcome axis used to split calibration examples.
    ///
    /// `None` preserves legacy profiles that pooled every adjudicable anchor.
    /// New high-stakes profiles should bind one axis so an execution failure,
    /// an OOD verdict, and a user reward cannot silently become the same label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_anchor_kind: Option<String>,
    pub tau: BTreeMap<SlotId, f32>,
    pub required_slots: Vec<SlotId>,
    pub policy: GuardPolicy,
    pub calibration: Option<CalibrationMeta>,
    pub novelty_action: NoveltyAction,
}

impl GuardProfile {
    /// Returns true when calibration provenance is attached.
    pub fn is_calibrated(&self) -> bool {
        self.calibration.is_some()
    }

    /// Returns the tau for `slot`; `None` means the slot is not guarded.
    pub fn tau_for(&self, slot: &SlotId) -> Option<f32> {
        self.tau.get(slot).copied()
    }
}

impl GuardTauProfile for GuardProfile {
    fn tau_for(&self, slot: &SlotId) -> Option<f32> {
        GuardProfile::tau_for(self, slot)
    }
}
