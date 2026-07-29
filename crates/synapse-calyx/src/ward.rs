//! Ward guard calibration and verification against the vault (#1677).
//!
//! # Why this module exists
//!
//! `calyx-search` already consumes a calibrated Ward [`GuardProfile`]: profile
//! backed `--guard in-region` loads the Guard CF row `profile\0default` and
//! fails closed with `CALYX_GUARD_PROVISIONAL` when it is absent. Until now no
//! `synapse-*` crate depended on `calyx-ward` at all, so nothing ever wrote that
//! row and the consumer could only ever fail. This module is the writer.
//!
//! # The calibration corpus contract, and why it is narrow on purpose
//!
//! Ward's tau is a **conformal / Clopper-Pearson** threshold: it is chosen as
//! the smallest cosine at which the *observed* false-accept count over a corpus
//! of KNOWN-BAD cases is low enough that the true false-accept rate is at most
//! `target_far` with confidence `1 - alpha` (Vovk et al.'s training-conditional
//! validity; the bound is the exact binomial inversion of Clopper-Pearson 1934).
//! That guarantee is a statement about a genuine bad-case distribution. Feed it
//! synthetic badness and the number it prints is not a weaker guarantee, it is
//! **no guarantee at all** — and it prints `ok` forever. So this module never
//! manufactures a bad case.
//!
//! The corpus is therefore drawn from exactly one thing the vault can state
//! about itself without interpretation: an [`Anchor`] whose value is
//! [`AnchorValue::Bool`] with `confidence > 0`. `Bool(true)` is an adjudicated
//! good outcome, `Bool(false)` an adjudicated bad one. Every other anchor value
//! (`Number`, `Enum`, `Text`, `OneHot`, `Vector`) has no repo-wide polarity
//! convention, so assigning it one here would be exactly the fabrication above:
//! those records are counted as *unadjudicated* and reported, never scored.
//!
//! The score function is the one the guard actually enforces with —
//! `dense_cosine(produced, matched)` — so calibration and deployment measure the
//! same quantity:
//!
//! - `bad_scores[j]`  = max cosine of bad record `j` against the good set.
//! - `good_scores[i]` = max cosine of good record `i` against the good set with
//!   itself removed (leave-one-out; including itself would score a constant 1.0
//!   and report a fictitious FRR of 0).
//!
//! # Where the corpus has to come from
//!
//! Synapse has no surface today that writes `Bool(false)` outcome anchors: no
//! adjudication/rejection path feeds a refuted outcome back onto a Base
//! constellation. Until one exists, `guard calibrate` on the production vault
//! will refuse with [`SYNAPSE_CALYX_GUARD_BAD_CORPUS_ABSENT`] naming that gap.
//! That refusal is the honest state. A guard that will not calibrate is safe; a
//! guard calibrated on invented badness is a lie with a confidence interval
//! printed next to it.

use std::collections::BTreeMap;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{AnchorValue, Clock, CxId, Panel, SlotId, SlotVector, Ts, dense_cosine};
use calyx_registry::load_vault_panel_state;
use calyx_ward::{
    CalibrationInput, GuardId, GuardPolicy, GuardProfile, MIN_BAD_SCORES, NoveltyAction, SlotKind,
    calibrate, guard, validate_calibration_slots,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

/// Guard CF key of the default calibrated profile.
///
/// This MUST stay byte-identical to
/// `calyx_search::engine::guard::DEFAULT_GUARD_PROFILE_KEY` (private to that
/// crate) — it is the single rendezvous point between this writer and the
/// profile-backed guarded-search reader.
pub const SYNAPSE_GUARD_DEFAULT_PROFILE_KEY: &[u8] = b"profile\0default";

/// Default conformal miscoverage budget: the calibrated tau bounds the true FAR
/// at `target_far` with confidence `1 - alpha`.
pub const SYNAPSE_GUARD_DEFAULT_ALPHA: f32 = 0.05;

/// Minimum good (in-region) exemplars needed per slot. Two is the arithmetic
/// floor for a leave-one-out nearest-neighbour score; it is not a statistical
/// sufficiency claim and is not reported as one.
pub const SYNAPSE_GUARD_MIN_GOOD_SCORES: usize = 2;

/// One slot to calibrate, with the aspect the operator asserts it carries.
///
/// The aspect is REQUIRED and never inferred: it sets the maximum permitted
/// `target_far` (`identity` 0.01, `content` 0.03, `stylistic` 0.05) and is
/// persisted into the profile's per-slot calibration metadata. Guessing it from
/// a slot id would silently relax or tighten the guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardSlotSpec {
    pub slot: u16,
    pub aspect: SynapseCalyxGuardAspect,
}

/// Operator-asserted slot aspect (mirrors `calyx_ward::SlotKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxGuardAspect {
    Identity,
    Stylistic,
    Content,
}

impl SynapseCalyxGuardAspect {
    #[must_use]
    pub const fn slot_kind(self) -> SlotKind {
        match self {
            Self::Identity => SlotKind::Identity,
            Self::Stylistic => SlotKind::Stylistic,
            Self::Content => SlotKind::Content,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        self.slot_kind().label()
    }
}

/// Bounded request for one guard calibration pass.
#[derive(Clone, Debug)]
pub struct SynapseCalyxGuardCalibrateParams {
    pub panel_version: u32,
    pub slots: Vec<SynapseCalyxGuardSlotSpec>,
    pub domain: String,
    pub alpha: f32,
    /// Per-slot target false-accept rate. `None` uses the aspect's maximum.
    pub target_far: Option<f32>,
    pub max_records: usize,
    /// When false the calibration is computed and reported but the Guard CF is
    /// not written (a dry run for operators sizing a corpus).
    pub persist: bool,
}

impl SynapseCalyxGuardCalibrateParams {
    #[must_use]
    pub fn new(panel_version: u32, slots: Vec<SynapseCalyxGuardSlotSpec>) -> Self {
        Self {
            panel_version,
            slots,
            domain: "default".to_owned(),
            alpha: SYNAPSE_GUARD_DEFAULT_ALPHA,
            target_far: None,
            max_records: crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            persist: true,
        }
    }
}

/// One slot's calibration evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardSlotCalibration {
    pub slot: u16,
    pub aspect: String,
    pub target_far: f32,
    pub tau: f32,
    /// Observed false-accept count at `tau` over the bad corpus.
    pub bad_accepts: usize,
    pub achieved_far: f64,
    pub achieved_frr: f64,
    pub good_scores: usize,
    pub bad_scores: usize,
    /// Exact one-sided Clopper-Pearson tail `P(X <= bad_accepts | n, target_far)`
    /// recomputed here as an independent readback of ward's own gate.
    pub clopper_pearson_tail: f64,
    /// Smallest bad-corpus size at which `target_far` is certifiable at `alpha`.
    pub certifiable_min_bad_scores: usize,
}

/// Result of one guard calibration pass with the physical Guard CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardCalibrateReport {
    pub panel_version: u32,
    pub domain: String,
    pub guard_id: String,
    pub alpha: f32,
    pub records_scanned: usize,
    pub adjudicated_good: usize,
    pub adjudicated_bad: usize,
    pub unadjudicated: usize,
    pub conflicting: usize,
    pub estimator: String,
    pub slots: Vec<SynapseCalyxGuardSlotCalibration>,
    pub persisted: bool,
    /// Byte length of the Guard CF row read back after the write.
    pub guard_cf_profile_bytes: usize,
    pub guard_cf_rows_after: usize,
    /// Proof the read-back row decodes as a calibrated profile.
    pub readback_calibrated: bool,
}

/// Bounded request for one guard verification.
#[derive(Clone, Debug)]
pub struct SynapseCalyxGuardVerifyParams {
    pub panel_version: u32,
    pub query_cx_id: String,
    pub high_stakes: bool,
    pub max_records: usize,
}

/// One slot's verdict inside a guard verification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardSlotVerdict {
    pub slot: u16,
    pub cos: f32,
    pub tau: f32,
    pub pass: bool,
    pub matched_cx_id: String,
}

/// A `calyx_ward::GuardVerdict` produced against the persisted profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxGuardVerifyReport {
    pub panel_version: u32,
    pub query_cx_id: String,
    pub guard_id: String,
    pub domain: String,
    pub high_stakes: bool,
    pub overall_pass: bool,
    pub provisional: bool,
    pub policy: String,
    pub required_slots: Vec<u16>,
    pub per_slot: Vec<SynapseCalyxGuardSlotVerdict>,
    pub failing_slots: Vec<u16>,
    pub action: Option<String>,
    pub calibration_far: Option<f32>,
    pub calibration_frr: Option<f32>,
    pub calibration_confidence: Option<f32>,
    pub trusted_exemplars: usize,
}

/// One record's adjudicated slot vectors.
struct AdjudicatedRecord {
    cx_id: CxId,
    slots: BTreeMap<u16, Vec<f32>>,
}

/// The adjudicated corpus split, with every excluded record counted.
struct AdjudicatedCorpus {
    good: Vec<AdjudicatedRecord>,
    bad: Vec<AdjudicatedRecord>,
    records_scanned: usize,
    unadjudicated: usize,
    conflicting: usize,
}

impl SynapseCalyxVault {
    /// Calibrates a Ward [`GuardProfile`] from the vault's adjudicated corpus and
    /// persists it to the native `Guard` CF under the key the guarded-search
    /// consumer reads, then reads that row back and re-decodes it.
    ///
    /// # Errors
    ///
    /// Fails closed — never with a fabricated corpus — when: the panel is not
    /// published; a requested slot is unknown/inactive/non-dense; there are no
    /// adjudicated bad cases (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_ABSENT`); there are
    /// fewer than [`MIN_BAD_SCORES`] of them
    /// (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_INSUFFICIENT`); the bad corpus is too
    /// small for `target_far` to be certifiable at `alpha`
    /// (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_UNCERTIFIABLE`); ward's own conformal
    /// gate did not actually certify the returned tau
    /// (`SYNAPSE_CALYX_GUARD_TAU_UNCERTIFIED`); or the CF write/readback fails.
    #[allow(clippy::too_many_lines)]
    pub fn guard_calibrate(
        &self,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> Result<SynapseCalyxGuardCalibrateReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("guard_calibrate");
        if params.slots.is_empty() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_NO_SLOTS",
                "guard calibration named no slots; a profile with no required slot is inert and would accept everything".to_owned(),
                "name at least one dense active panel slot with its aspect (identity/stylistic/content)",
            ));
        }
        if !params.alpha.is_finite() || !(0.0..1.0).contains(&params.alpha) {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_INVALID_ALPHA",
                format!(
                    "guard calibration alpha {} is not a finite miscoverage budget in [0,1)",
                    params.alpha
                ),
                "supply a conformal miscoverage budget such as 0.05 (95% confidence)",
            ));
        }
        let panel = self.active_panel_for_guard(params.panel_version)?;
        let corpus = self.collect_adjudicated_corpus(params)?;

        // The honesty gate. Refusing is the correct outcome when reality has not
        // supplied a bad-case distribution; there is no synthetic branch.
        if corpus.bad.is_empty() {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_BAD_CORPUS_ABSENT",
                format!(
                    "panel {} has no adjudicated bad case: {} record(s) scanned, {} adjudicated good (anchor Bool(true), confidence > 0), {} adjudicated bad (anchor Bool(false), confidence > 0), {} unadjudicated (anchors carrying Number/Enum/Text/OneHot/Vector values, which have no repo-wide good/bad polarity and are never interpreted as one)",
                    params.panel_version,
                    corpus.records_scanned,
                    corpus.good.len(),
                    corpus.bad.len(),
                    corpus.unadjudicated
                ),
                "a conformal FAR bound is only meaningful over a real known-bad distribution. Synapse has no surface today that writes a refuted outcome (AnchorValue::Bool(false)) back onto a Base constellation, so this corpus must come from a real adjudication path — human review, a deterministic oracle, or a replayed incident set — before the guard can be calibrated. Calibrating on manufactured badness would report `ok` forever and is refused here, not worked around",
            ));
        }

        let clock = FixedGuardClock(self.clock_now_ms()?);
        let mut inputs = Vec::with_capacity(params.slots.len());
        let mut evidence = Vec::with_capacity(params.slots.len());
        for spec in &params.slots {
            let target_far = params
                .target_far
                .unwrap_or_else(|| spec.aspect.slot_kind().default_target_far());
            let good_scores = leave_one_out_scores(&corpus.good, spec.slot);
            let bad_scores = nearest_good_scores(&corpus.bad, &corpus.good, spec.slot);
            if good_scores.len() < SYNAPSE_GUARD_MIN_GOOD_SCORES {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_GOOD_CORPUS_INSUFFICIENT",
                    format!(
                        "slot {} has {} adjudicated good exemplar(s) with a dense vector; a leave-one-out in-region score needs at least {SYNAPSE_GUARD_MIN_GOOD_SCORES}",
                        spec.slot,
                        good_scores.len()
                    ),
                    "embed this slot on more Bool(true)-anchored records, or drop the slot from the calibration request",
                ));
            }
            if bad_scores.len() < MIN_BAD_SCORES {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_BAD_CORPUS_INSUFFICIENT",
                    format!(
                        "slot {} has {} adjudicated bad score(s) with a dense vector; ward's conformal calibration requires at least {MIN_BAD_SCORES}",
                        spec.slot,
                        bad_scores.len()
                    ),
                    "collect more adjudicated bad cases for this panel; do not lower the minimum, and do not synthesize cases to reach it",
                ));
            }
            let certifiable_min = min_certifiable_bad_scores(target_far, params.alpha);
            if bad_scores.len() < certifiable_min {
                // Without this gate ward's conformal search finds no certifiable
                // candidate and falls back to `next_above(max bad score)`, which
                // rejects every possible cosine while still reporting far=0.0 at
                // the requested confidence. That silent conservative fallback
                // reads as a calibrated guard. Refuse instead, and name n.
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_BAD_CORPUS_UNCERTIFIABLE",
                    format!(
                        "slot {} has {} adjudicated bad score(s); target_far {target_far} cannot be certified at alpha {} with fewer than {certifiable_min} (the exact one-sided Clopper-Pearson bound needs (1-target_far)^n <= alpha even with zero false accepts)",
                        spec.slot,
                        bad_scores.len(),
                        params.alpha
                    ),
                    "collect at least the named number of adjudicated bad cases, or raise target_far to what this corpus size can actually certify",
                ));
            }

            inputs.push(CalibrationInput {
                slot: SlotId::new(spec.slot),
                good_scores,
                bad_scores,
                slot_kind: spec.aspect.slot_kind(),
                target_far,
            });
            evidence.push((*spec, target_far, certifiable_min));
        }

        validate_calibration_slots(&inputs, &panel).map_err(|error| {
            guard_error(
                error.code(),
                format!("guard calibration slot validation failed: {error}"),
                "name only dense, Active slots of the published panel; a profile guarding a sparse/multi/parked slot fails every query at query time",
            )
        })?;

        let template = GuardProfile {
            guard_id: GuardId::new(Uuid::new_v4()),
            panel_version: params.panel_version,
            domain: params.domain.clone(),
            tau: BTreeMap::new(),
            required_slots: Vec::new(),
            policy: GuardPolicy::AllRequired,
            calibration: None,
            novelty_action: NoveltyAction::RejectClosed,
        };
        let profile = calibrate(template, inputs.clone(), params.alpha, &clock).map_err(|error| {
            guard_error(
                error.code(),
                format!("ward conformal calibration failed: {error}"),
                "inspect the reported corpus counts; never relax the calibration minimums to force a profile",
            )
        })?;

        let mut slots = Vec::with_capacity(inputs.len());
        for (input, (spec, target_far, certifiable_min)) in inputs.iter().zip(evidence) {
            let tau = profile.tau_for(&input.slot).ok_or_else(|| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_TAU_MISSING",
                    format!("ward returned no tau for calibrated slot {}", spec.slot),
                    "inspect calyx-ward calibrate(); a calibrated profile must carry a tau per input slot",
                )
            })?;
            let bad_accepts = input
                .bad_scores
                .iter()
                .filter(|score| **score >= tau)
                .count();
            let tail = clopper_pearson_tail(bad_accepts, input.bad_scores.len(), target_far);
            // Independent readback of ward's own gate: if the returned tau is
            // not certified at alpha, ward took its conservative fallback and
            // the profile's reported FAR is not a bound. Refuse to persist it.
            if tail > f64::from(params.alpha) + f64::EPSILON {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_TAU_UNCERTIFIED",
                    format!(
                        "slot {} tau {tau} accepts {bad_accepts}/{} bad case(s); the exact one-sided Clopper-Pearson tail P(X <= {bad_accepts} | n, {target_far}) = {tail} exceeds alpha {}, so this tau is NOT certified at the requested confidence",
                        spec.slot,
                        input.bad_scores.len(),
                        params.alpha
                    ),
                    "the calibration corpus does not support the requested target_far/alpha; collect more adjudicated bad cases or relax target_far — a profile whose tau is uncertified is never persisted",
                ));
            }
            let far_at_tau = fraction(bad_accepts, input.bad_scores.len());
            let good_reject_rate = fraction(
                input
                    .good_scores
                    .iter()
                    .filter(|score| **score < tau)
                    .count(),
                input.good_scores.len(),
            );
            slots.push(SynapseCalyxGuardSlotCalibration {
                slot: spec.slot,
                aspect: spec.aspect.label().to_owned(),
                target_far,
                tau,
                bad_accepts,
                achieved_far: far_at_tau,
                achieved_frr: good_reject_rate,
                good_scores: input.good_scores.len(),
                bad_scores: input.bad_scores.len(),
                clopper_pearson_tail: tail,
                certifiable_min_bad_scores: certifiable_min,
            });
        }

        let (guard_cf_profile_bytes, readback_calibrated) = if params.persist {
            let encoded = serde_json::to_vec(&profile).map_err(|error| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_ENCODE_FAILED",
                    format!("encode the calibrated guard profile: {error}"),
                    "inspect the profile fields before retrying the calibration",
                )
            })?;
            self.write_cf_batch(vec![SynapseCalyxCfWrite {
                cf: ColumnFamily::Guard,
                key: SYNAPSE_GUARD_DEFAULT_PROFILE_KEY.to_vec(),
                value: encoded,
            }])?;
            self.flush()?;
            let Some(row) =
                self.read_cf_latest(ColumnFamily::Guard, SYNAPSE_GUARD_DEFAULT_PROFILE_KEY)?
            else {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_READBACK_MISSING",
                    "the calibrated guard profile is absent from the Guard CF immediately after a flushed write".to_owned(),
                    "inspect the vault Guard CF and the commit path; a write that cannot be read back is never reported as persisted",
                ));
            };
            let decoded: GuardProfile = serde_json::from_slice(&row).map_err(|error| {
                guard_error(
                    "SYNAPSE_CALYX_GUARD_PROFILE_READBACK_DECODE_FAILED",
                    format!("decode the Guard CF profile that was just written: {error}"),
                    "the persisted row is not a GuardProfile the guarded-search consumer can load; inspect the codec",
                )
            })?;
            (
                row.len(),
                decoded.is_calibrated() && decoded.guard_id == profile.guard_id,
            )
        } else {
            (0, false)
        };
        let guard_cf_rows_after = self.scan_cf_latest(ColumnFamily::Guard)?.len();

        Ok(SynapseCalyxGuardCalibrateReport {
            panel_version: params.panel_version,
            domain: params.domain.clone(),
            guard_id: profile.guard_id.to_string(),
            alpha: params.alpha,
            records_scanned: corpus.records_scanned,
            adjudicated_good: corpus.good.len(),
            adjudicated_bad: corpus.bad.len(),
            unadjudicated: corpus.unadjudicated,
            conflicting: corpus.conflicting,
            estimator: calyx_ward::ESTIMATOR.to_owned(),
            slots,
            persisted: params.persist,
            guard_cf_profile_bytes,
            guard_cf_rows_after,
            readback_calibrated,
        })
    }

    /// Verifies one record against the persisted Ward guard profile and returns
    /// the full [`calyx_ward::GuardVerdict`] decomposition.
    ///
    /// # Errors
    ///
    /// Fails closed when no calibrated profile is persisted, when the profile
    /// was calibrated for a different panel, when the query record or its
    /// required slot vectors are missing, or when no trusted exemplar exists to
    /// match against.
    #[allow(clippy::too_many_lines)]
    pub fn guard_verify(
        &self,
        params: &SynapseCalyxGuardVerifyParams,
    ) -> Result<SynapseCalyxGuardVerifyReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("guard_verify");
        let query_cx = crate::parse_cx_id(&params.query_cx_id)?;
        let Some(row) =
            self.read_cf_latest(ColumnFamily::Guard, SYNAPSE_GUARD_DEFAULT_PROFILE_KEY)?
        else {
            return Err(guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                "no calibrated Ward guard profile is persisted (Guard CF key `profile\\0default` is absent)".to_owned(),
                "run guard calibrate for this panel; guarded search and guard verification both fail closed rather than guess a tau",
            ));
        };
        let profile: GuardProfile = serde_json::from_slice(&row).map_err(|error| {
            guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!("decode the persisted default guard profile: {error}"),
                "the Guard CF row is not a decodable GuardProfile; recalibrate the guard",
            )
        })?;
        if profile.panel_version != params.panel_version {
            return Err(guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                format!(
                    "guard profile panel_version {} does not match the requested panel {}",
                    profile.panel_version, params.panel_version
                ),
                "recalibrate the guard for the active panel before verifying against it",
            ));
        }
        if !profile.is_calibrated() {
            return Err(guard_error(
                calyx_ward::CALYX_GUARD_PROVISIONAL,
                "the persisted default guard profile carries no calibration provenance".to_owned(),
                "run guard calibrate; an uncalibrated profile is never used to admit output",
            ));
        }

        let slot_specs: Vec<SynapseCalyxGuardSlotSpec> = profile
            .required_slots
            .iter()
            .map(|slot| SynapseCalyxGuardSlotSpec {
                slot: slot.get(),
                // Aspect is irrelevant to verification (only the corpus split
                // and the dense vectors are used); Content is the neutral
                // placeholder and is never persisted from this path.
                aspect: SynapseCalyxGuardAspect::Content,
            })
            .collect();
        let scan = SynapseCalyxGuardCalibrateParams {
            panel_version: params.panel_version,
            slots: slot_specs,
            domain: profile.domain.clone(),
            alpha: SYNAPSE_GUARD_DEFAULT_ALPHA,
            target_far: None,
            max_records: params.max_records,
            persist: false,
        };
        let corpus = self.collect_adjudicated_corpus(&scan)?;
        let query = self.load_record_slots(params.panel_version, query_cx, &scan)?;

        let mut produced = BTreeMap::new();
        let mut matched = BTreeMap::new();
        let mut matched_ids = BTreeMap::new();
        for slot in &profile.required_slots {
            let raw = slot.get();
            let Some(query_vec) = query.get(&raw) else {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_QUERY_SLOT_MISSING",
                    format!(
                        "record {query_cx} carries no dense vector for required guard slot {raw} in panel {}",
                        params.panel_version
                    ),
                    "supply a record that carries every slot the profile requires; a missing required slot is never treated as a pass",
                ));
            };
            let Some((exemplar, _)) = best_match(query_vec, &corpus.good, raw) else {
                return Err(guard_error(
                    "SYNAPSE_CALYX_GUARD_NO_TRUSTED_EXEMPLAR",
                    format!(
                        "no adjudicated good exemplar carries slot {raw} in panel {}; there is nothing trusted to match the record against",
                        params.panel_version
                    ),
                    "anchor in-region records with AnchorValue::Bool(true) and embed the guarded slots before verifying",
                ));
            };
            let exemplar_vec = corpus.good[exemplar]
                .slots
                .get(&raw)
                .cloned()
                .unwrap_or_default();
            matched_ids.insert(raw, corpus.good[exemplar].cx_id.to_string());
            produced.insert(*slot, query_vec.clone());
            matched.insert(*slot, exemplar_vec);
        }

        let verdict =
            guard(&profile, &produced, &matched, params.high_stakes).map_err(|error| {
                guard_error(
                    error.code(),
                    format!("ward guard evaluation failed: {error}"),
                    "inspect the profile's required slots and the record's slot coverage",
                )
            })?;

        let per_slot: Vec<SynapseCalyxGuardSlotVerdict> = verdict
            .per_slot
            .iter()
            .map(|slot| SynapseCalyxGuardSlotVerdict {
                slot: slot.slot.get(),
                cos: slot.cos,
                tau: slot.tau,
                pass: slot.pass,
                matched_cx_id: matched_ids
                    .get(&slot.slot.get())
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();

        Ok(SynapseCalyxGuardVerifyReport {
            panel_version: params.panel_version,
            query_cx_id: query_cx.to_string(),
            guard_id: verdict.guard_id.to_string(),
            domain: profile.domain.clone(),
            high_stakes: params.high_stakes,
            overall_pass: verdict.overall_pass,
            provisional: verdict.provisional,
            policy: match profile.policy {
                GuardPolicy::AllRequired => "all_required".to_owned(),
                GuardPolicy::KofN { k } => format!("k_of_n(k={k})"),
            },
            required_slots: profile
                .required_slots
                .iter()
                .copied()
                .map(SlotId::get)
                .collect(),
            failing_slots: per_slot
                .iter()
                .filter(|slot| !slot.pass)
                .map(|slot| slot.slot)
                .collect(),
            per_slot,
            action: verdict.action.map(|action| match action {
                NoveltyAction::NewRegion => "new_region".to_owned(),
                NoveltyAction::Quarantine => "quarantine".to_owned(),
                NoveltyAction::RejectClosed => "reject_closed".to_owned(),
            }),
            calibration_far: profile.calibration.as_ref().map(|meta| meta.far),
            calibration_frr: profile.calibration.as_ref().map(|meta| meta.frr),
            calibration_confidence: profile.calibration.as_ref().map(|meta| meta.confidence),
            trusted_exemplars: corpus.good.len(),
        })
    }

    /// Loads the published active [`Panel`] the guard must be calibrated for.
    fn active_panel_for_guard(&self, panel_version: u32) -> Result<Panel, SynapseCalyxError> {
        let state = load_vault_panel_state(&self.config.vault_dir).map_err(|error| {
            guard_error(
                "SYNAPSE_CALYX_GUARD_NO_ACTIVE_PANEL",
                format!("no durable active panel is published for guard calibration: {error}"),
                "publish the active panel for this constellation before calibrating a guard profile",
            )
        })?;
        if state.panel.version != panel_version {
            return Err(guard_error(
                "SYNAPSE_CALYX_GUARD_PANEL_MISMATCH",
                format!(
                    "guard calibration requested panel {panel_version}, but the durable active panel is {}",
                    state.panel.version
                ),
                "calibrate against the published active panel; a profile bound to another panel fails every guarded query",
            ));
        }
        Ok(state.panel)
    }

    /// Splits the panel into adjudicated good/bad records, counting everything
    /// it refuses to interpret.
    fn collect_adjudicated_corpus(
        &self,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> Result<AdjudicatedCorpus, SynapseCalyxError> {
        let max_records = params
            .max_records
            .clamp(1, crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let wanted: Vec<u16> = params.slots.iter().map(|spec| spec.slot).collect();
        let mut corpus = AdjudicatedCorpus {
            good: Vec::new(),
            bad: Vec::new(),
            records_scanned: 0,
            unadjudicated: 0,
            conflicting: 0,
        };
        for (_, value) in self.scan_cf_latest(ColumnFamily::Base)? {
            let constellation = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if constellation.panel_version != params.panel_version {
                continue;
            }
            corpus.records_scanned += 1;
            let mut good = false;
            let mut bad = false;
            let mut adjudicated = false;
            for anchor in &constellation.anchors {
                if anchor.confidence <= 0.0 {
                    continue;
                }
                match anchor.value {
                    AnchorValue::Bool(true) => {
                        good = true;
                        adjudicated = true;
                    }
                    AnchorValue::Bool(false) => {
                        bad = true;
                        adjudicated = true;
                    }
                    // No repo-wide polarity convention exists for these values.
                    // Interpreting one would fabricate the very labels the
                    // conformal bound is a statement about.
                    _ => {}
                }
            }
            if !adjudicated {
                corpus.unadjudicated += 1;
                continue;
            }
            if good && bad {
                corpus.conflicting += 1;
                continue;
            }
            let mut slots = BTreeMap::new();
            for slot in &wanted {
                if let Some(vector) = constellation
                    .slots
                    .get(&SlotId::new(*slot))
                    .and_then(guard_dense_vector)
                {
                    slots.insert(*slot, vector);
                }
            }
            if slots.is_empty() {
                continue;
            }
            let record = AdjudicatedRecord {
                cx_id: constellation.cx_id,
                slots,
            };
            if good {
                corpus.good.push(record);
            } else {
                corpus.bad.push(record);
            }
            if corpus.records_scanned >= max_records {
                break;
            }
        }
        Ok(corpus)
    }

    /// Loads one record's dense vectors for the requested slots.
    fn load_record_slots(
        &self,
        panel_version: u32,
        cx_id: CxId,
        params: &SynapseCalyxGuardCalibrateParams,
    ) -> Result<BTreeMap<u16, Vec<f32>>, SynapseCalyxError> {
        for (_, value) in self.scan_cf_latest(ColumnFamily::Base)? {
            let constellation = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if constellation.panel_version != panel_version || constellation.cx_id != cx_id {
                continue;
            }
            let mut slots = BTreeMap::new();
            for spec in &params.slots {
                if let Some(vector) = constellation
                    .slots
                    .get(&SlotId::new(spec.slot))
                    .and_then(guard_dense_vector)
                {
                    slots.insert(spec.slot, vector);
                }
            }
            return Ok(slots);
        }
        Err(guard_error(
            "SYNAPSE_CALYX_GUARD_QUERY_RECORD_MISSING",
            format!("record {cx_id} is not present in panel {panel_version}"),
            "supply a cx_id that exists in this panel",
        ))
    }
}

/// Leave-one-out nearest-neighbour cosine per good exemplar: the in-region score
/// the guard would compute for that record matched to its nearest OTHER trusted
/// exemplar. Including itself would score a constant 1.0 and report FRR = 0.
fn leave_one_out_scores(good: &[AdjudicatedRecord], slot: u16) -> Vec<f32> {
    let mut scores = Vec::new();
    for (index, record) in good.iter().enumerate() {
        let Some(vector) = record.slots.get(&slot) else {
            continue;
        };
        let mut best: Option<f32> = None;
        for (other_index, other) in good.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let Some(other_vector) = other.slots.get(&slot) else {
                continue;
            };
            if let Some(cos) = dense_cosine(vector, other_vector) {
                best = Some(best.map_or(cos, |current: f32| current.max(cos)));
            }
        }
        if let Some(score) = best {
            scores.push(score);
        }
    }
    scores
}

/// Nearest-good cosine per bad case: the score the guard would compute if that
/// known-bad output were presented and matched to the trusted region.
fn nearest_good_scores(
    bad: &[AdjudicatedRecord],
    good: &[AdjudicatedRecord],
    slot: u16,
) -> Vec<f32> {
    bad.iter()
        .filter_map(|record| record.slots.get(&slot))
        .filter_map(|vector| best_match(vector, good, slot).map(|(_, score)| score))
        .collect()
}

/// Index of and cosine to the best-matching good exemplar on `slot`.
fn best_match(vector: &[f32], good: &[AdjudicatedRecord], slot: u16) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (index, record) in good.iter().enumerate() {
        let Some(candidate) = record.slots.get(&slot) else {
            continue;
        };
        let Some(cos) = dense_cosine(vector, candidate) else {
            continue;
        };
        if best.is_none_or(|(_, current)| cos > current) {
            best = Some((index, cos));
        }
    }
    best
}

/// Upper bound on the search for a certifiable corpus size. Past this the
/// requested `target_far`/`alpha` pair is reported as uncertifiable rather than
/// searched forever.
const MAX_CERTIFIABLE_SEARCH: usize = 10_000_000;

/// Smallest bad-corpus size at which `target_far` is certifiable at `alpha`.
///
/// The exact one-sided Clopper-Pearson upper bound with zero observed false
/// accepts requires `(1 - target_far)^n <= alpha` — the "rule of three" made
/// exact. Solved here by repeated multiplication so no float is ever narrowed
/// to an integer. With `target_far = 0.01` and `alpha = 0.05` the answer is
/// 299: `0.99^298 = 0.050037 > 0.05` and `0.99^299 = 0.049536 <= 0.05`.
#[must_use]
pub fn min_certifiable_bad_scores(target_far: f32, alpha: f32) -> usize {
    let target = f64::from(target_far);
    let alpha = f64::from(alpha);
    if !(0.0..1.0).contains(&target) || target <= 0.0 || alpha <= 0.0 {
        return usize::MAX;
    }
    if alpha >= 1.0 {
        return 1;
    }
    let survival_step = 1.0 - target;
    let mut survival = survival_step;
    for n in 1..=MAX_CERTIFIABLE_SEARCH {
        if survival <= alpha {
            return n;
        }
        survival *= survival_step;
    }
    usize::MAX
}

/// Exact binomial tail `P(X <= successes | trials, probability)`.
///
/// This is the tail the one-sided Clopper-Pearson upper bound inverts, and it is
/// recomputed here as an INDEPENDENT readback of the gate `calyx_ward::calibrate`
/// applies internally — a returned tau whose tail exceeds `alpha` was ward's
/// conservative fallback, not a certified threshold.
#[must_use]
pub fn clopper_pearson_tail(successes: usize, trials: usize, probability: f32) -> f64 {
    let probability = f64::from(probability);
    if successes >= trials {
        return 1.0;
    }
    if probability <= 0.0 {
        return 1.0;
    }
    if probability >= 1.0 {
        return 0.0;
    }
    let complement = 1.0 - probability;
    let trials_f = count_as_f64(trials);
    let mut term = complement.powi(i32::try_from(trials).unwrap_or(i32::MAX));
    let mut sum = term;
    for index in 0..successes {
        let index = count_as_f64(index);
        term *= (trials_f - index) / (index + 1.0) * probability / complement;
        sum += term;
        if sum > 1.0 {
            return 1.0;
        }
    }
    sum
}

/// Widens a corpus count to `f64` without a lossy primitive cast. Counts are
/// bounded by the scan record cap, far below `u32::MAX`.
fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

fn guard_dense_vector(vector: &SlotVector) -> Option<Vec<f32>> {
    match vector {
        SlotVector::Dense { data, .. } => Some(data.clone()),
        SlotVector::Sparse { .. } | SlotVector::Multi { .. } | SlotVector::Absent { .. } => None,
    }
}

fn fraction(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        count_as_f64(count) / count_as_f64(total)
    }
}

fn guard_error(
    code: &'static str,
    message: String,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message, remediation)
}

/// The vault's own millisecond clock, sampled once so every calibration record
/// in one pass carries the same timestamp.
struct FixedGuardClock(Ts);

impl Clock for FixedGuardClock {
    fn now(&self) -> Ts {
        self.0
    }
}
