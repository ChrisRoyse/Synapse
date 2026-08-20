//! Persisted chronological held-out evidence for action-domain readiness.

use std::collections::{BTreeMap, BTreeSet};

use calyx_anneal::{GoodhartReport, GoodhartViolation, RegressionReport, RegressionResult};
use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::{AnchorKind, AnchorValue, CxId, SlotId, SlotVector};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use calyx_ward::{GuardPolicy, GuardProfile};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{SynapseCalyxError, SynapseCalyxVault, SynapseCalyxWalkStep};

pub const ACTION_DOMAIN: &str = "synapse.action";
/// Physical `AnnealReport` row holding the held-out action validation report.
///
/// `v2` because the evidence now binds the exact Ward profile revision it was
/// scored against (`guard_profile_sha256`). A `v1` row cannot answer "was the
/// Goodhart boundary recalibrated after this was measured", so it is not
/// upgraded in place: it is simply not read, and readiness reports absent
/// evidence with a rerun remediation instead of a false pass.
pub const ACTION_VALIDATION_KEY: &[u8] = b"oracle-validation/v3/synapse.action";
pub const ACTION_VALIDATION_SCHEMA_VERSION: u32 = 3;
/// Must track `SYN_ACTION_PANEL_VERSION` in
/// `synapse-storage/src/constellations.rs` (no dependency edge exists in this
/// direction, so the value is duplicated by hand). A mismatch fails loud
/// (`SYNAPSE_CALYX_ACTION_VALIDATION_PANEL_MISMATCH`) rather than reading
/// evidence measured under a different frozen slot layout: bumped `2_185_005`
/// -> `2_185_006` to add the compact admission-context lane and separate Ward's
/// OOD calibration axis from command reward. This deliberately re-arms
/// readiness — held-out evidence and the Ward boundary must be re-measured on
/// the causally coherent population, never inherited from the memorizing exact
/// request boundary. Bumped `2_185_006` -> `2_185_007` after physical guard
/// calibration proved that the structural v1 admission lens collapsed
/// different good/bad scalar requests. The causal predictor now consumes the
/// immutable value-aware slot 124 and leaves slot 123 as readable history.
pub const ACTION_PANEL_VERSION: u32 = 2_185_007;
pub const ACTION_GUARD_ANCHOR_KIND: &str = "action_guard_region";
const ACTION_CAUSAL_PREDICTOR: &str = "typed_slot_rrf_knn.v1";
const ACTION_CAUSAL_PREDICTOR_SLOTS: &[u16] = &[48, 117, 118, 119, 120, 121, 122, 124];
const MIN_ACTION_RECORDS: usize = 50;
pub const MIN_HELD_OUT_RECORDS: usize = 10;
const MAX_ACTION_RECORDS: usize = 20_000;
const MAX_HELD_OUT_RECORDS: usize = 200;
const MAX_GUARD_TRAINING_RECORDS: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxActionValidationEvidence {
    pub schema_version: u32,
    pub domain: String,
    pub panel_version: u32,
    pub measured_at_seq: u64,
    pub action_record_count: usize,
    pub action_corpus_sha256: String,
    pub held_out_count: usize,
    pub held_out_sha256: String,
    pub guard_training_successes: usize,
    pub guard_held_out_successes: usize,
    /// SHA-256 of the exact `Guard` CF profile bytes the Goodhart holdout was
    /// scored against. Readiness refuses evidence whose boundary has since been
    /// recalibrated, because an in-region fraction only means anything relative
    /// to the profile that produced it.
    pub guard_profile_sha256: String,
    pub regression_evaluated: usize,
    pub mistake_count: usize,
    pub predictor: String,
    pub predictor_slots: Vec<u16>,
    pub goodhart: GoodhartReport,
    pub mistakes: RegressionReport,
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StoredActionValidation {
    evidence: SynapseCalyxActionValidationEvidence,
    evidence_sha256: String,
}

#[derive(Clone, Debug)]
struct ActionObservation {
    cx_id: CxId,
    created_at: u64,
    action: String,
    outcome: bool,
    causes: BTreeMap<SlotId, Vec<f32>>,
}

impl SynapseCalyxVault {
    /// Builds and atomically persists held-out action validation plus its native
    /// Anneal ledger binding. Readiness never generates this evidence itself.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the requested panel is invalid, the held-out
    /// corpus cannot be measured, or the committed evidence cannot be read back.
    #[expect(
        clippy::too_many_lines,
        reason = "validation, atomic persistence, and independent readback form one ordered evidence transaction"
    )]
    pub fn validate_action_readiness(
        &self,
        panel_version: u32,
    ) -> Result<SynapseCalyxActionValidationEvidence, SynapseCalyxError> {
        if panel_version != ACTION_PANEL_VERSION {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_PANEL_MISMATCH",
                format!(
                    "action validation requires panel {ACTION_PANEL_VERSION}, got {panel_version}"
                ),
                "run oracle_validate against the declared syn-action-v1 panel",
            ));
        }
        let measured_at_seq = self.latest_seq();
        let observations = self.action_observations()?;
        if observations.len() < MIN_ACTION_RECORDS {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_INSUFFICIENT",
                format!(
                    "action validation found {} grounded action records; at least {MIN_ACTION_RECORDS} are required",
                    observations.len()
                ),
                "collect real terminal action outcomes before validating autonomy",
            ));
        }
        let held_out_count =
            (observations.len() / 5).clamp(MIN_HELD_OUT_RECORDS, MAX_HELD_OUT_RECORDS);
        let split = observations.len() - held_out_count;
        let (training, held_out) = observations.split_at(split);
        let corpus_hash = action_corpus_hash(&observations);
        let held_out_hash = action_corpus_hash(held_out);
        let (goodhart, guard_training_successes, guard_held_out_successes, guard_profile_sha256) =
            self.action_goodhart_report(panel_version, training, held_out)?;
        let (mistakes, regression_evaluated, mistake_count) =
            action_mistake_report(training, held_out, &observations)?;

        let draft = SynapseCalyxActionValidationEvidence {
            schema_version: ACTION_VALIDATION_SCHEMA_VERSION,
            domain: ACTION_DOMAIN.to_owned(),
            panel_version,
            measured_at_seq,
            action_record_count: observations.len(),
            action_corpus_sha256: corpus_hash.clone(),
            held_out_count,
            held_out_sha256: held_out_hash.clone(),
            guard_training_successes,
            guard_held_out_successes,
            guard_profile_sha256: guard_profile_sha256.clone(),
            regression_evaluated,
            mistake_count,
            predictor: ACTION_CAUSAL_PREDICTOR.to_owned(),
            predictor_slots: ACTION_CAUSAL_PREDICTOR_SLOTS.to_vec(),
            goodhart,
            mistakes,
            ledger_seq: 0,
            ledger_hash: String::new(),
        };
        let payload = serde_json::to_vec(&serde_json::json!({
            "schema_version": ACTION_VALIDATION_SCHEMA_VERSION,
            "panel_version": panel_version,
            "measured_at_seq": measured_at_seq,
            "action_record_count": observations.len(),
            "action_corpus_sha256": corpus_hash,
            "held_out_count": held_out_count,
            "held_out_sha256": held_out_hash,
            "guard_profile_sha256": guard_profile_sha256,
            "goodhart_passed": draft.goodhart.passed,
            "mistakes_passed": draft.mistakes.passed,
            "mistake_count": mistake_count,
            "predictor": ACTION_CAUSAL_PREDICTOR,
            "predictor_slots": ACTION_CAUSAL_PREDICTOR_SLOTS,
        }))
        .map_err(|error| validation_encode_error("ledger payload", &error))?;
        let mut persisted: Option<SynapseCalyxActionValidationEvidence> = None;
        self.vault
            .append_ledger_entry_with_rows(
                EntryKind::Anneal,
                SubjectId::Query(ACTION_DOMAIN.as_bytes().to_vec()),
                payload,
                ActorId::Service("synapse-action-validator".to_owned()),
                |ledger_ref| {
                    let mut evidence = draft.clone();
                    evidence.ledger_seq = ledger_ref.seq;
                    evidence.ledger_hash = hex(&ledger_ref.hash);
                    let evidence_bytes = serde_json::to_vec(&evidence).map_err(|error| {
                        calyx_core::CalyxError::ledger_group_commit_failed(format!(
                            "encode action validation evidence: {error}"
                        ))
                    })?;
                    let stored = StoredActionValidation {
                        evidence: evidence.clone(),
                        evidence_sha256: hex(&Sha256::digest(&evidence_bytes)),
                    };
                    let value = serde_json::to_vec(&stored).map_err(|error| {
                        calyx_core::CalyxError::ledger_group_commit_failed(format!(
                            "encode stored action validation: {error}"
                        ))
                    })?;
                    persisted = Some(evidence);
                    Ok(vec![(
                        ColumnFamily::AnnealReport,
                        ACTION_VALIDATION_KEY.to_vec(),
                        value,
                    )])
                },
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "atomically persist action validation and Anneal ledger",
                    &error,
                )
            })?;
        let expected = persisted.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_COMMIT_MISSING",
                "action validation commit returned without materializing its evidence row",
                "preserve the vault and inspect the ledger group-commit closure",
            )
        })?;
        let actual = self.read_action_validation()?.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_READBACK_MISSING",
                "action validation row is absent immediately after its atomic commit",
                "inspect the AnnealReport CF and WAL durability",
            )
        })?;
        if actual != expected {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_READBACK_MISMATCH",
                "action validation readback differs from the atomically committed evidence",
                "preserve the vault and inspect the AnnealReport row revision",
            ));
        }
        Ok(actual)
    }

    /// Reads the latest durable held-out action validation evidence.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical evidence row cannot be read or decoded.
    pub fn read_action_validation(
        &self,
    ) -> Result<Option<SynapseCalyxActionValidationEvidence>, SynapseCalyxError> {
        Ok(self
            .read_action_validation_revisioned()?
            .map(|(evidence, _)| evidence))
    }

    /// Reads the evidence together with the SHA-256 revision of the exact
    /// physical row it came from, so a readiness snapshot can name the row it
    /// measured and an operator can independently confirm the same row.
    pub(crate) fn read_action_validation_revisioned(
        &self,
    ) -> Result<Option<(SynapseCalyxActionValidationEvidence, String)>, SynapseCalyxError> {
        let Some(row) =
            self.read_cf_latest_revisioned(ColumnFamily::AnnealReport, ACTION_VALIDATION_KEY)?
        else {
            return Ok(None);
        };
        let bytes = row.value;
        let row_revision_sha256 = hex(&row.revision_sha256);
        let stored: StoredActionValidation = serde_json::from_slice(&bytes).map_err(|error| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CORRUPT",
                format!("decode action validation row: {error}"),
                "quarantine the corrupt AnnealReport row and rerun oracle_validate",
            )
        })?;
        let evidence_bytes = serde_json::to_vec(&stored.evidence)
            .map_err(|error| validation_encode_error("readback evidence", &error))?;
        let actual_hash = hex(&Sha256::digest(&evidence_bytes));
        if actual_hash != stored.evidence_sha256 {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_INTEGRITY_FAILED",
                format!(
                    "action validation evidence hash {actual_hash} != stored {}",
                    stored.evidence_sha256
                ),
                "quarantine the tampered AnnealReport row and rerun oracle_validate",
            ));
        }
        Ok(Some((stored.evidence, row_revision_sha256)))
    }

    pub(crate) fn current_action_corpus_binding(
        &self,
    ) -> Result<(usize, String), SynapseCalyxError> {
        let observations = self.action_observations()?;
        Ok((observations.len(), action_corpus_hash(&observations)))
    }

    /// Digests the action panel's live Ward profile row exactly as validation
    /// digested it, so readiness can prove the Goodhart boundary has not moved
    /// since the held-out report was scored. `None` means the profile row is
    /// physically absent.
    pub(crate) fn current_guard_profile_sha256(
        &self,
        panel_version: u32,
    ) -> Result<Option<String>, SynapseCalyxError> {
        Ok(self
            .read_cf_latest(ColumnFamily::Guard, &guard_profile_key(panel_version))?
            .map(|bytes| hex(&Sha256::digest(&bytes))))
    }

    fn action_observations(&self) -> Result<Vec<ActionObservation>, SynapseCalyxError> {
        let mut observations = Vec::new();
        self.with_panel_read_snapshot(
            ACTION_PANEL_VERSION,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| self.walk_panel_base_snapshot(snapshot, ACTION_PANEL_VERSION, |snapshot, _key, value| {
            let base = decode_constellation_base(value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode action validation Base row", &error)
            })?;
            if base.panel_version != ACTION_PANEL_VERSION
                || base.metadata.get("oracle.domain").map(String::as_str) != Some(ACTION_DOMAIN)
            {
                return Ok(SynapseCalyxWalkStep::Continue);
            }
            let action = base
                .metadata
                .get("oracle.action")
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_ACTION_MISSING",
                    format!("action record {} has no oracle.action identity", base.cx_id),
                    "repair the source action publication; validation never guesses an action identity",
                ))?;
            let outcomes = base
                .anchors
                .iter()
                .filter(|anchor| anchor.kind == AnchorKind::Reward && anchor.confidence > 0.0)
                .filter_map(|anchor| match anchor.value { AnchorValue::Bool(value) => Some(value), _ => None })
                .collect::<BTreeSet<_>>();
            if outcomes.is_empty() {
                return Ok(SynapseCalyxWalkStep::Continue);
            }
            if outcomes.len() != 1 {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_OUTCOME_INVALID",
                    format!("action record {} has {} distinct grounded Bool reward outcomes", base.cx_id, outcomes.len()),
                    "repair the action outcome anchors; validation requires exactly one unambiguous terminal result",
                ));
            }
            let outcome = *outcomes.iter().next().ok_or_else(|| validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_OUTCOME_MISSING",
                format!("action record {} has no grounded Bool reward", base.cx_id),
                "publish the real terminal action outcome before validation",
            ))?;
            let hydrated = self.hydrated_constellation_at_snapshot(base.cx_id, snapshot)?;
            let mut causes = BTreeMap::new();
            for raw_slot in ACTION_CAUSAL_PREDICTOR_SLOTS {
                let slot = SlotId::new(*raw_slot);
                if let Some(SlotVector::Dense { data, .. }) = hydrated.slots.get(&slot)
                    && !data.is_empty()
                    && data.iter().all(|value| value.is_finite())
                {
                    causes.insert(slot, data.clone());
                }
            }
            if !causes.contains_key(&SlotId::new(124)) {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_ADMISSION_CONTEXT_MISSING",
                    format!("action reward record {} lacks finite dense value-aware admission-context slot 124", base.cx_id),
                    "repair the action-panel backfill before validating autonomy; the causal predictor never falls back to action-name majority",
                ));
            }
            observations.push(ActionObservation { cx_id: base.cx_id, created_at: base.created_at, action: action.to_owned(), outcome, causes });
            if observations.len() > MAX_ACTION_RECORDS {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_CORPUS_LIMIT",
                    format!("action corpus exceeds the bounded {MAX_ACTION_RECORDS}-record validation budget"),
                    "add a versioned incremental validation window before enabling autonomy on a larger corpus",
                ));
            }
            Ok(SynapseCalyxWalkStep::Continue)
        }),
        )?;
        observations.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.cx_id.cmp(&right.cx_id))
        });
        Ok(observations)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one held-out measurement must retain its calibrated profile, corpus split, violations, and exact profile revision"
    )]
    fn action_goodhart_report(
        &self,
        panel_version: u32,
        training: &[ActionObservation],
        held_out: &[ActionObservation],
    ) -> Result<(GoodhartReport, usize, usize, String), SynapseCalyxError> {
        let profile_key = guard_profile_key(panel_version);
        let bytes = self.read_cf_latest(ColumnFamily::Guard, &profile_key)?.ok_or_else(|| validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_GUARD_ABSENT",
            format!("no panel-keyed Ward profile exists for action panel {panel_version}"),
            "calibrate the action-panel guard from real good and bad cases before oracle_validate",
        ))?;
        let guard_profile_sha256 = hex(&Sha256::digest(&bytes));
        let profile: GuardProfile = serde_json::from_slice(&bytes).map_err(|error| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_GUARD_CORRUPT",
                format!("decode action-panel Ward profile: {error}"),
                "recalibrate the corrupt action-panel guard",
            )
        })?;
        if profile.panel_version != panel_version
            || !profile.is_calibrated()
            || profile.required_slots.is_empty()
            || profile.calibration_anchor_kind.as_deref() != Some(ACTION_GUARD_ANCHOR_KIND)
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_GUARD_PROVISIONAL",
                "action-panel Ward profile is uncalibrated, mismatched, has no required slots, or is not bound to action_guard_region",
                "calibrate a non-empty per-slot action guard with anchor_kind=action_guard_region before oracle_validate",
            ));
        }
        let training_good = training
            .iter()
            .filter(|row| row.outcome)
            .rev()
            .take(MAX_GUARD_TRAINING_RECORDS)
            .collect::<Vec<_>>();
        let held_out_good = held_out
            .iter()
            .filter(|row| row.outcome)
            .collect::<Vec<_>>();
        if training_good.len() < MIN_HELD_OUT_RECORDS || held_out_good.len() < MIN_HELD_OUT_RECORDS
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_GOODHART_INSUFFICIENT",
                format!(
                    "Goodhart guard holdout has {} training successes and {} held-out successes",
                    training_good.len(),
                    held_out_good.len()
                ),
                "collect at least ten real successful actions on both sides of the chronological split",
            ));
        }
        let mut trusted = Vec::with_capacity(training_good.len());
        for row in training_good {
            trusted.push(self.action_dense_slots(row.cx_id, &profile.required_slots)?);
        }
        let mut accepted = 0usize;
        for row in &held_out_good {
            let query = self.action_dense_slots(row.cx_id, &profile.required_slots)?;
            let pass_count = profile
                .required_slots
                .iter()
                .filter(|slot| {
                    let Some(query_vector) = query.get(slot) else {
                        return false;
                    };
                    let Some(tau) = profile.tau_for(slot) else {
                        return false;
                    };
                    trusted
                        .iter()
                        .filter_map(|candidate| candidate.get(slot))
                        .filter_map(|candidate| dense_cosine(query_vector, candidate))
                        .max_by(f32::total_cmp)
                        .is_some_and(|score| score >= tau)
                })
                .count();
            let passes = match &profile.policy {
                GuardPolicy::AllRequired => pass_count == profile.required_slots.len(),
                GuardPolicy::KofN { k } => pass_count >= *k,
            };
            accepted += usize::from(passes);
        }
        let accepted = accepted.to_f64().ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_COUNT_OUT_OF_RANGE",
                format!("accepted held-out count {accepted} cannot be represented as f64"),
                "reduce the held-out corpus below the platform floating-point conversion limit",
            )
        })?;
        let held_out_count = held_out_good.len().to_f64().ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_COUNT_OUT_OF_RANGE",
                format!(
                    "held-out corpus count {} cannot be represented as f64",
                    held_out_good.len()
                ),
                "reduce the held-out corpus below the platform floating-point conversion limit",
            )
        })?;
        let in_region_frac = accepted / held_out_count;
        let mut violations = Vec::new();
        if in_region_frac < f64::from(calyx_oracle::GOODHART_THRESHOLD) {
            violations.push(GoodhartViolation::GtauViolation {
                in_region_frac,
                threshold: f64::from(calyx_oracle::GOODHART_THRESHOLD),
            });
        }
        Ok((
            GoodhartReport {
                passed: violations.is_empty(),
                violations,
                p_goodhart_increment: 0.0,
                j_train_delta: 0.0,
                j_heldout_delta: Some(0.0),
                in_region_frac: Some(in_region_frac),
                warnings: vec![
                    "chronological_action_success_holdout_scored_against_training_only".to_owned(),
                ],
            },
            trusted.len(),
            held_out_good.len(),
            guard_profile_sha256,
        ))
    }

    fn action_dense_slots(
        &self,
        cx_id: CxId,
        required: &[SlotId],
    ) -> Result<BTreeMap<SlotId, Vec<f32>>, SynapseCalyxError> {
        let cx = self.hydrate_constellation_latest(cx_id)?;
        let mut slots = BTreeMap::new();
        for slot in required {
            let vector = match cx.slots.get(slot) {
                Some(SlotVector::Dense { data, .. })
                    if !data.is_empty() && data.iter().all(|value| value.is_finite()) =>
                {
                    data.clone()
                }
                _ => {
                    return Err(validation_error(
                        "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_MISSING",
                        format!(
                            "action record {cx_id} lacks finite dense required slot {}",
                            slot.get()
                        ),
                        "repair action-panel backfill before validating autonomy",
                    ));
                }
            };
            slots.insert(*slot, vector);
        }
        Ok(slots)
    }
}

fn action_mistake_report(
    training: &[ActionObservation],
    held_out: &[ActionObservation],
    all: &[ActionObservation],
) -> Result<(RegressionReport, usize, usize), SynapseCalyxError> {
    let mut prior = training.iter().collect::<Vec<_>>();
    let mut results = Vec::new();
    let mut evaluated = 0usize;
    for row in held_out {
        if let Some(old) = typed_causal_prediction(row, prior.iter().copied())? {
            evaluated += 1;
            if old != row.outcome {
                let now = typed_causal_prediction(row, all.iter())?;
                let (new_prediction, new_surprise, recurred, prediction_error) = now.map_or_else(
                    || {
                        (
                            f64::from(u8::from(old)),
                            1.0,
                            true,
                            Some(
                                "typed causal predictor has no non-tied current neighbor evidence"
                                    .to_owned(),
                            ),
                        )
                    },
                    |now| {
                        (
                            f64::from(u8::from(now)),
                            if now == row.outcome { 0.0 } else { 1.0 },
                            now != row.outcome,
                            None,
                        )
                    },
                );
                results.push(RegressionResult {
                    cx_id: row.cx_id,
                    old_prediction: f64::from(u8::from(old)),
                    observed: f64::from(u8::from(row.outcome)),
                    old_surprise: 1.0,
                    new_prediction,
                    new_surprise,
                    recurred,
                    anchor: AnchorKind::Reward,
                    prediction_error,
                });
            }
        }
        prior.push(row);
    }
    if evaluated < MIN_HELD_OUT_RECORDS {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_REPLAY_INSUFFICIENT",
            format!("only {evaluated} held-out actions had prior evidence for real replay"),
            "collect terminal outcomes with comparable pre-trigger causal slots before validating autonomy",
        ));
    }
    let mistake_count = results.len();
    Ok((RegressionReport::new(results), evaluated, mistake_count))
}

fn typed_causal_prediction<'a>(
    query: &ActionObservation,
    candidates: impl IntoIterator<Item = &'a ActionObservation>,
) -> Result<Option<bool>, SynapseCalyxError> {
    const RRF_K: f64 = 60.0;
    const TOP_NEIGHBORS: usize = 11;
    let candidates = candidates
        .into_iter()
        .filter(|candidate| candidate.cx_id != query.cx_id)
        .collect::<Vec<_>>();
    let mut fused: BTreeMap<CxId, (f64, bool)> = BTreeMap::new();
    for raw_slot in ACTION_CAUSAL_PREDICTOR_SLOTS {
        let slot = SlotId::new(*raw_slot);
        let Some(query_vector) = query.causes.get(&slot) else {
            continue;
        };
        let mut ranking = candidates
            .iter()
            .filter_map(|candidate| {
                let score = dense_cosine(query_vector, candidate.causes.get(&slot)?)?;
                (score > 0.0).then_some((score, candidate.cx_id, candidate.outcome))
            })
            .collect::<Vec<_>>();
        ranking.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        for (rank, (_, cx_id, outcome)) in ranking.into_iter().enumerate() {
            let rank = rank.to_f64().ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_RANK_OUT_OF_RANGE",
                    "causal neighbor rank cannot be represented as f64",
                    "reduce the bounded action validation corpus",
                )
            })? + 1.0;
            let contribution = 1.0 / (RRF_K + rank);
            let entry = fused.entry(cx_id).or_insert((0.0, outcome));
            if entry.1 != outcome {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_OUTCOME_IDENTITY_CONFLICT",
                    format!("causal neighbor {cx_id} carries conflicting outcomes"),
                    "repair the action corpus; one content-addressed observation cannot carry two outcomes",
                ));
            }
            entry.0 += contribution;
        }
    }
    let mut ranking = fused
        .into_iter()
        .map(|(cx_id, (score, outcome))| (score, cx_id, outcome))
        .collect::<Vec<_>>();
    ranking.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    let mut failed = 0.0;
    let mut succeeded = 0.0;
    for (score, _, outcome) in ranking.into_iter().take(TOP_NEIGHBORS) {
        if outcome {
            succeeded += score;
        } else {
            failed += score;
        }
    }
    if succeeded == 0.0 && failed == 0.0 || (succeeded - failed).abs() <= f64::EPSILON {
        return Ok(None);
    }
    Ok(Some(succeeded > failed))
}

fn action_corpus_hash(rows: &[ActionObservation]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-action-validation-corpus-v2-typed-slot-rrf-knn");
    for row in rows {
        hasher.update(row.cx_id.as_bytes());
        hasher.update(row.created_at.to_be_bytes());
        hasher.update((row.action.len() as u64).to_be_bytes());
        hasher.update(row.action.as_bytes());
        hasher.update([u8::from(row.outcome)]);
    }
    hex(&hasher.finalize())
}

fn guard_profile_key(panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(19);
    key.extend_from_slice(b"profile\0panel\0");
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn dense_cosine(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (&a, &b) in left.iter().zip(right) {
        dot = f64::from(a).mul_add(f64::from(b), dot);
        left_norm = f64::from(a).mul_add(f64::from(a), left_norm);
        right_norm = f64::from(b).mul_add(f64::from(b), right_norm);
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return None;
    }
    (dot / (left_norm.sqrt() * right_norm.sqrt()))
        .clamp(-1.0, 1.0)
        .to_f32()
}

fn validation_encode_error(context: &str, error: &serde_json::Error) -> SynapseCalyxError {
    validation_error(
        "SYNAPSE_CALYX_ACTION_VALIDATION_ENCODE_FAILED",
        format!("encode action validation {context}: {error}"),
        "inspect the action validation schema and finite numeric invariants",
    )
}

fn validation_error(
    code: &'static str,
    message: impl Into<String>,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message, remediation)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
