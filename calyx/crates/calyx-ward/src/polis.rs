//! Polis civic-panel guard validation over deterministic synthetic personas.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use calyx_core::SlotId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::WardError;
use crate::guard::{MatchedSlots, ProducedSlots, guard};
use crate::profile::{
    CalibrationMeta, GuardId, GuardPolicy, GuardProfile, NoveltyAction, SlotCalibrationMeta,
};
use crate::verdict::GuardVerdict;

pub const CIVIC_SLOT_COUNT: usize = 21;
pub const CIVIC_TAU: f32 = 0.7;
const SCHEMA_VERSION: u64 = 1;
const SURFACE: &str = "ph70-polis-civic-guard-fsv";
const SOURCE_OF_TRUTH: &str = "PH70 Polis civic synthetic-persona guard/tie artifact";
const GUARD_UUID: &str = "61100000-7070-7000-8000-000000000611";
const CALIBRATION_TS: i64 = 1_786_147_200;

pub const CALYX_POLIS_EMPTY_PERSONA_SET: &str = "CALYX_POLIS_EMPTY_PERSONA_SET";
pub const CALYX_POLIS_SLOT_COUNT_MISMATCH: &str = "CALYX_POLIS_SLOT_COUNT_MISMATCH";
pub const CALYX_POLIS_INVALID_AXIS: &str = "CALYX_POLIS_INVALID_AXIS";
pub const CALYX_POLIS_TIE_MISMATCH: &str = "CALYX_POLIS_TIE_MISMATCH";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CivicPersona {
    pub persona_id: String,
    pub axes: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CivicPersonaPair {
    pub pair_id: String,
    pub left: CivicPersona,
    pub right: CivicPersona,
    pub planted_tie: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolisCivicProof {
    pub schema_version: u64,
    pub surface: &'static str,
    pub source_of_truth: &'static str,
    pub panel_name: &'static str,
    pub civic_slot_count: usize,
    pub temporal_slots_excluded: Vec<u16>,
    pub tau: f32,
    pub required_slots: Vec<u16>,
    pub calibration_corpus_sha256: String,
    pub all_expected_outcomes_match: bool,
    pub pairs: Vec<PairProof>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairProof {
    pub pair_id: String,
    pub planted_tie: bool,
    pub actual_tie: bool,
    pub tie_outcome_matches: bool,
    pub axis_agreements: usize,
    pub failing_slots: Vec<u16>,
    pub verdict: GuardVerdict,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PolisCivicError {
    EmptyPersonaSet,
    SlotCountMismatch {
        pair_id: String,
        persona_id: String,
        expected: usize,
        actual: usize,
    },
    InvalidAxis {
        pair_id: String,
        persona_id: String,
        slot: u16,
        value: f32,
    },
    TieMismatch {
        pair_id: String,
        expected: bool,
        actual: bool,
    },
    Ward(WardError),
}

impl PolisCivicError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EmptyPersonaSet => CALYX_POLIS_EMPTY_PERSONA_SET,
            Self::SlotCountMismatch { .. } => CALYX_POLIS_SLOT_COUNT_MISMATCH,
            Self::InvalidAxis { .. } => CALYX_POLIS_INVALID_AXIS,
            Self::TieMismatch { .. } => CALYX_POLIS_TIE_MISMATCH,
            Self::Ward(error) => error.code(),
        }
    }
}

impl fmt::Display for PolisCivicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPersonaSet => write!(
                f,
                "{CALYX_POLIS_EMPTY_PERSONA_SET}: synthetic persona pair set is empty"
            ),
            Self::SlotCountMismatch {
                pair_id,
                persona_id,
                expected,
                actual,
            } => write!(
                f,
                "{CALYX_POLIS_SLOT_COUNT_MISMATCH}: pair {pair_id} persona {persona_id} has {actual} civic axes, expected {expected}"
            ),
            Self::InvalidAxis {
                pair_id,
                persona_id,
                slot,
                value,
            } => write!(
                f,
                "{CALYX_POLIS_INVALID_AXIS}: pair {pair_id} persona {persona_id} slot {slot} has invalid axis value {value}"
            ),
            Self::TieMismatch {
                pair_id,
                expected,
                actual,
            } => write!(
                f,
                "{CALYX_POLIS_TIE_MISMATCH}: pair {pair_id} expected_tie={expected} actual_tie={actual}"
            ),
            Self::Ward(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl Error for PolisCivicError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ward(error) => Some(error),
            _ => None,
        }
    }
}

impl From<WardError> for PolisCivicError {
    fn from(value: WardError) -> Self {
        Self::Ward(value)
    }
}

pub fn evaluate_polis_civic_pairs(
    pairs: &[CivicPersonaPair],
) -> Result<PolisCivicProof, PolisCivicError> {
    if pairs.is_empty() {
        return Err(PolisCivicError::EmptyPersonaSet);
    }
    for pair in pairs {
        validate_persona(pair, &pair.left)?;
        validate_persona(pair, &pair.right)?;
    }

    let corpus_hash = corpus_hash(pairs);
    let profile = civic_profile(corpus_hash);
    let mut proofs = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let produced = persona_slots(&pair.right);
        let matched = persona_slots(&pair.left);
        let verdict = guard(&profile, &produced, &matched, true)?;
        let actual_tie = verdict.overall_pass;
        if actual_tie != pair.planted_tie {
            return Err(PolisCivicError::TieMismatch {
                pair_id: pair.pair_id.clone(),
                expected: pair.planted_tie,
                actual: actual_tie,
            });
        }
        proofs.push(PairProof {
            pair_id: pair.pair_id.clone(),
            planted_tie: pair.planted_tie,
            actual_tie,
            tie_outcome_matches: true,
            axis_agreements: axis_agreements(&pair.left.axes, &pair.right.axes),
            failing_slots: verdict
                .failing_slots()
                .into_iter()
                .map(|slot| slot.slot.get())
                .collect(),
            verdict,
        });
    }

    Ok(PolisCivicProof {
        schema_version: SCHEMA_VERSION,
        surface: SURFACE,
        source_of_truth: SOURCE_OF_TRUTH,
        panel_name: "civic-default",
        civic_slot_count: CIVIC_SLOT_COUNT,
        temporal_slots_excluded: vec![22, 23, 24],
        tau: CIVIC_TAU,
        required_slots: required_slots().into_iter().map(SlotId::get).collect(),
        calibration_corpus_sha256: hex32(corpus_hash),
        all_expected_outcomes_match: true,
        pairs: proofs,
    })
}

pub fn synthetic_polis_persona_pairs() -> Vec<CivicPersonaPair> {
    let alpha = persona("alpha", alternating_axes(false));
    let beta = persona("beta", alternating_axes(false));
    let gamma = persona("gamma", vec![1.0; CIVIC_SLOT_COUNT]);
    let delta = persona("delta", vec![1.0; CIVIC_SLOT_COUNT]);
    let mut epsilon_axes = alternating_axes(false);
    epsilon_axes[6] *= -1.0;
    let epsilon = persona("epsilon", epsilon_axes);
    let mut zeta_axes = vec![1.0; CIVIC_SLOT_COUNT];
    for value in zeta_axes.iter_mut().take(11) {
        *value *= -1.0;
    }
    let zeta = persona("zeta", zeta_axes);

    vec![
        pair("tie-alpha-beta", alpha.clone(), beta, true),
        pair("tie-gamma-delta", gamma.clone(), delta, true),
        pair("reject-single-axis-07", alpha, epsilon, false),
        pair("reject-majority-shift", gamma, zeta, false),
    ]
}

fn civic_profile(corpus_hash: [u8; 32]) -> GuardProfile {
    let mut tau = BTreeMap::new();
    let mut per_slot = BTreeMap::new();
    for slot in required_slots() {
        tau.insert(slot, CIVIC_TAU);
        per_slot.insert(
            slot,
            SlotCalibrationMeta {
                corpus_hash,
                estimator: "polis-synthetic-sign".to_string(),
                far: 0.0,
                frr: 0.0,
                confidence: 1.0,
                ts: CALIBRATION_TS,
                slot_kind: None,
            },
        );
    }
    GuardProfile {
        guard_id: GUARD_UUID.parse::<GuardId>().expect("static guard id"),
        panel_version: 70,
        domain: "polis-civic-synthetic-personas".to_string(),
        calibration_anchor_kind: None,
        tau,
        required_slots: required_slots(),
        policy: GuardPolicy::AllRequired,
        calibration: Some(CalibrationMeta {
            corpus_hash,
            estimator: "polis-synthetic-sign".to_string(),
            far: 0.0,
            frr: 0.0,
            confidence: 1.0,
            ts: CALIBRATION_TS,
            per_slot,
        }),
        novelty_action: NoveltyAction::Quarantine,
    }
}

fn validate_persona(
    pair: &CivicPersonaPair,
    persona: &CivicPersona,
) -> Result<(), PolisCivicError> {
    if persona.axes.len() != CIVIC_SLOT_COUNT {
        return Err(PolisCivicError::SlotCountMismatch {
            pair_id: pair.pair_id.clone(),
            persona_id: persona.persona_id.clone(),
            expected: CIVIC_SLOT_COUNT,
            actual: persona.axes.len(),
        });
    }
    for (index, value) in persona.axes.iter().copied().enumerate() {
        if !value.is_finite() || value == 0.0 {
            return Err(PolisCivicError::InvalidAxis {
                pair_id: pair.pair_id.clone(),
                persona_id: persona.persona_id.clone(),
                slot: (index + 1) as u16,
                value,
            });
        }
    }
    Ok(())
}

fn persona_slots(persona: &CivicPersona) -> ProducedSlots {
    persona
        .axes
        .iter()
        .copied()
        .enumerate()
        .map(|(index, value)| (SlotId::new((index + 1) as u16), vec![value]))
        .collect()
}

fn required_slots() -> Vec<SlotId> {
    (1..=CIVIC_SLOT_COUNT)
        .map(|index| SlotId::new(index as u16))
        .collect()
}

fn axis_agreements(left: &[f32], right: &[f32]) -> usize {
    left.iter()
        .zip(right)
        .filter(|(left, right)| left.signum() == right.signum())
        .count()
}

fn corpus_hash(pairs: &[CivicPersonaPair]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for pair in pairs {
        hasher.update(pair.pair_id.as_bytes());
        hasher.update([pair.planted_tie as u8]);
        hash_persona(&mut hasher, &pair.left);
        hash_persona(&mut hasher, &pair.right);
    }
    hasher.finalize().into()
}

fn hash_persona(hasher: &mut Sha256, persona: &CivicPersona) {
    hasher.update(persona.persona_id.as_bytes());
    for value in &persona.axes {
        hasher.update(value.to_le_bytes());
    }
}

fn alternating_axes(invert: bool) -> Vec<f32> {
    (0..CIVIC_SLOT_COUNT)
        .map(|index| {
            let sign = if index % 2 == 0 { 1.0 } else { -1.0 };
            if invert { -sign } else { sign }
        })
        .collect()
}

fn persona(persona_id: &str, axes: Vec<f32>) -> CivicPersona {
    CivicPersona {
        persona_id: persona_id.to_string(),
        axes,
    }
}

fn pair(
    pair_id: &str,
    left: CivicPersona,
    right: CivicPersona,
    planted_tie: bool,
) -> CivicPersonaPair {
    CivicPersonaPair {
        pair_id: pair_id.to_string(),
        left,
        right,
        planted_tie,
    }
}

fn hex32(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[allow(dead_code)]
fn _matched_slots_type_check(slots: ProducedSlots) -> MatchedSlots {
    slots
}
