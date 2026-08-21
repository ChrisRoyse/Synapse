//! Persisted chronological held-out evidence for action-domain readiness.

use std::collections::{BTreeMap, BTreeSet};

use calyx_anneal::{GoodhartReport, GoodhartViolation, RegressionReport, RegressionResult};
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_aster::{
    cf::ColumnFamily,
    collection::{
        Collection, CollectionMode, DedupPolicy, RetentionPolicy, TemporalPolicy, TenantId,
        TxnPolicy,
    },
    layers::{BlobId, BlobLayer},
};
use calyx_core::{AnchorKind, AnchorValue, CxId, SlotId, SlotVector};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use calyx_ward::{GuardPolicy, GuardProfile};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    SynapseCalyxCausalViewLifecycle, SynapseCalyxCausalViewRegistryScope, SynapseCalyxError,
    SynapseCalyxGuardVerifyParams, SynapseCalyxVault, SynapseCalyxWalkStep,
};

pub const ACTION_DOMAIN: &str = "synapse.action";
/// Physical `AnnealReport` row holding the held-out action validation report.
///
/// `v6` adds the immutable compact causal predictor contract and independently
/// hash-binds collection coverage for resource-prestate slot 136. Older rows
/// remain historical at their old keys; they are never inferred or upgraded.
pub const ACTION_VALIDATION_KEY: &[u8] = b"oracle-validation/v6/synapse.action";
pub const ACTION_VALIDATION_SCHEMA_VERSION: u32 = 6;
/// Must track `SYN_ACTION_PANEL_VERSION` in
/// `synapse-storage/src/constellations.rs` (no dependency edge exists in this
/// direction, so the value is duplicated by hand). A mismatch fails loud
/// (`SYNAPSE_CALYX_ACTION_VALIDATION_PANEL_MISMATCH`) rather than reading
/// evidence measured under a different frozen slot layout. Generation
/// `2_260_001` derives bounded, typed compact views from the immutable request
/// and pre-trigger admission facts while retaining the opaque historical slots
/// as audit/guard evidence. Slot 136 collects resource-prestate without joining
/// the serving predictor until a later powered cohort is immutably promoted.
pub const ACTION_PANEL_VERSION: u32 = 2_260_001;
pub const ACTION_GUARD_ANCHOR_KIND: &str = "action_guard_region";
/// Frozen target-population contract for chronological validation.
///
/// Slot 125 is intentionally `Absent` for source rows written before the
/// writer-sealed `synapse.shell_admission_facts.v1` snapshot existed. Those
/// rows cannot be repaired without inventing point-in-time validator facts.
/// Validation therefore measures the explicitly named complete-cause cohort,
/// while binding and surfacing the entire excluded identity set. This is not
/// imputation or a fallback to weaker slots.
pub const ACTION_CAUSAL_POPULATION_CONTRACT: &str =
    "reward_dense_admission_v3_slot125_compact_views_v1_resource136_collection_only";
pub const ACTION_CAUSAL_PREDICTOR: &str = "typed_slot_rrf_knn.v4.lowered-compact-causes.guarded.support3.separation005.rrf-k60.slot-top64.final-top11";
/// One representation per admitted cause.
///
/// Exact request identity (118), the
/// target-only projection (117), opaque precondition projection (122), and
/// their nested conjunction (125) remain durable audit evidence but are
/// not counted again: compact request atom slot 121 already carries the target
/// field when applicable and remains defined for target-independent actions.
///
/// The ordered roster is part of the predictor digest.
pub const ACTION_CAUSAL_PREDICTOR_SLOTS: &[u16] = &[
    48, 119, 120, 121, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135,
];
/// Frozen query-local OOD roster for the action Guard.
///
/// This is deliberately the minimal writer-sealed shell-rejection roster, not
/// an alias for every predictor cause. Its atoms correspond to the public
/// `action_guard_region=false` routes: command shape (126), environment state
/// (127), request identity policy (131), allow-shell policy (132), and
/// executable resolution (133). The joint `AllRequired` calibration lets a
/// heterogeneous bad case collide on unrelated atoms while still requiring at
/// least one physical rejection cause to fall outside the trusted region.
/// Contextual causes without a physically adjudicated bad route remain typed
/// predictor inputs and audit evidence; adding them here would assert a Guard
/// calibration population that the writer does not actually produce.
pub const ACTION_CAUSAL_GUARD_SLOTS: &[u16] = &[126, 127, 131, 132, 133];
const ACTION_COMPLETE_CAUSE_SEAL_SLOT: u16 = 125;
const ACTION_RESOURCE_CAUSE_SLOT: u16 = 136;
pub const ACTION_CAUSAL_REGISTRY_SERVING_SLOTS: &[u16] =
    &[126, 127, 128, 129, 130, 131, 132, 133, 134, 135];
const MIN_ACTION_RECORDS: usize = 50;
pub const MIN_HELD_OUT_RECORDS: usize = 10;
const MAX_ACTION_RECORDS: usize = 20_000;
const MAX_HELD_OUT_RECORDS: usize = 200;
const MAX_GUARD_TRAINING_RECORDS: usize = 1_000;
const MAX_EXCLUDED_DIAGNOSTIC_SAMPLE: usize = 16;
/// Bounded per-slot funnel before reciprocal-rank fusion. Every candidate is
/// still scored once against each typed cause, but only each slot's strongest
/// finite-support neighborhood is sorted and fused. This caps both ranking CPU
/// and the number of candidate contributions without allocating an ANN index.
const MAX_RRF_NEIGHBORS_PER_SLOT: usize = 64;
const ACTION_RRF_K: u32 = 60;
const ACTION_FINAL_NEIGHBORS: usize = 11;
/// A terminal prediction needs at least three independently persisted outcome
/// records after the per-slot funnel. One matching row is retrieval evidence,
/// not a grounded causal neighborhood.
const MIN_ACTION_PREDICTION_SOURCES: usize = 3;
/// Frozen normalized vote margin. Tiny floating-point differences are an
/// `Insufficient` verdict, never a categorical outcome.
const MIN_ACTION_PREDICTION_SEPARATION: f64 = 0.05;
const MIN_ACTION_PREDICTION_CONFIDENCE: f64 = 0.05;
const ACTION_PREDICTOR_ARTIFACT_SCHEMA_VERSION: u32 = 1;
const ACTION_PREDICTOR_ARTIFACT_MAX_BYTES: usize = 96 * 1024 * 1024;
const ACTION_PREDICTOR_BLOB_COLLECTION: &str = "synapse-action-predictor-v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxActionValidationEvidence {
    pub schema_version: u32,
    pub domain: String,
    pub panel_version: u32,
    pub measured_at_seq: u64,
    /// Exact panel-scoped Base/slot watermark pinned by the corpus scan.  This
    /// is the O(1) serving freshness precondition: unrelated vault writes do
    /// not invalidate action evidence, while any action-panel mutation does.
    pub panel_content_seq: u64,
    /// Exact O(1) Anchors-CF signal bracketing the validation scan. New
    /// unanchored intent rows do not move it; any grounded outcome mutation
    /// does, invalidating the lowered predictor without a serving-time scan.
    pub anchors_cf_last_commit_seq: u64,
    pub anchors_cf_out_of_band_epoch: u64,
    pub causal_population_contract: String,
    /// Every grounded reward row encountered before causal eligibility is
    /// applied. This equals `action_record_count +
    /// excluded_incomplete_causal_records`.
    pub source_reward_record_count: usize,
    /// Grounded reward rows used by the chronological split. Every one carries
    /// finite dense slot 125; no missing cause is imputed.
    pub action_record_count: usize,
    pub action_corpus_sha256: String,
    /// Rows retained for history but excluded because the writer had not yet
    /// sealed the complete admission snapshot required by slot 125.
    pub excluded_incomplete_causal_records: usize,
    pub excluded_incomplete_causal_sha256: String,
    /// Bounded diagnostic sample; the complete excluded set is committed by
    /// `excluded_incomplete_causal_sha256`.
    pub excluded_incomplete_causal_sample: Vec<String>,
    /// Eligible complete-admission rows that also carry the newly collected
    /// resource-prestate cause. Slot 136 remains collection-only until this
    /// independently hash-bound cohort is powered and promoted by a later
    /// immutable predictor contract.
    pub resource_cause_record_count: usize,
    pub resource_cause_record_sha256: String,
    pub resource_cause_missing_records: usize,
    pub resource_cause_missing_sha256: String,
    pub causal_registry_sha256: String,
    pub causal_registry_catalog_sha256: String,
    pub causal_registry_serving_slots: Vec<u16>,
    pub causal_registry_serving_slots_sha256: String,
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
    /// Content hash of the complete executable predictor contract, including
    /// ordered slot roster, rank-fusion semantics, and Registry catalog.
    pub predictor_sha256: String,
    /// Content-addressed Calyx Blob holding the exact prepared grounded corpus
    /// replayed by validation and consumed by production prediction.
    pub predictor_artifact_blob_id: String,
    pub predictor_artifact_blake3: String,
    pub predictor_artifact_total_bytes: u64,
    pub predictor_artifact_manifest_seq: u64,
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

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ActionValidationLedgerPayload<'a> {
    tag: &'static str,
    /// Digest of every evidence field except the ledger reference itself.
    /// This binds the full Goodhart/regression reports and all diagnostic
    /// counts, not merely the summary predicates below.
    evidence_content_sha256: String,
    schema_version: u32,
    domain: &'a str,
    panel_version: u32,
    measured_at_seq: u64,
    panel_content_seq: u64,
    anchors_cf_last_commit_seq: u64,
    anchors_cf_out_of_band_epoch: u64,
    causal_population_contract: &'a str,
    source_reward_record_count: usize,
    action_record_count: usize,
    action_corpus_sha256: &'a str,
    excluded_incomplete_causal_records: usize,
    excluded_incomplete_causal_sha256: &'a str,
    excluded_incomplete_causal_sample: &'a [String],
    resource_cause_record_count: usize,
    resource_cause_record_sha256: &'a str,
    resource_cause_missing_records: usize,
    resource_cause_missing_sha256: &'a str,
    causal_registry_sha256: &'a str,
    causal_registry_catalog_sha256: &'a str,
    causal_registry_serving_slots: &'a [u16],
    causal_registry_serving_slots_sha256: &'a str,
    held_out_count: usize,
    held_out_sha256: &'a str,
    guard_profile_sha256: &'a str,
    goodhart_passed: bool,
    mistakes_passed: bool,
    mistake_count: usize,
    predictor: &'a str,
    predictor_slots: &'a [u16],
    predictor_sha256: &'a str,
    predictor_artifact_blob_id: &'a str,
    predictor_artifact_blake3: &'a str,
    predictor_artifact_total_bytes: u64,
    predictor_artifact_manifest_seq: u64,
}

fn action_validation_ledger_payload(
    evidence: &SynapseCalyxActionValidationEvidence,
) -> Result<Vec<u8>, SynapseCalyxError> {
    serde_json::to_vec(&ActionValidationLedgerPayload {
        tag: "synapse-action-validation-ledger-v2",
        evidence_content_sha256: action_validation_evidence_content_sha256(evidence)?,
        schema_version: evidence.schema_version,
        domain: &evidence.domain,
        panel_version: evidence.panel_version,
        measured_at_seq: evidence.measured_at_seq,
        panel_content_seq: evidence.panel_content_seq,
        anchors_cf_last_commit_seq: evidence.anchors_cf_last_commit_seq,
        anchors_cf_out_of_band_epoch: evidence.anchors_cf_out_of_band_epoch,
        causal_population_contract: &evidence.causal_population_contract,
        source_reward_record_count: evidence.source_reward_record_count,
        action_record_count: evidence.action_record_count,
        action_corpus_sha256: &evidence.action_corpus_sha256,
        excluded_incomplete_causal_records: evidence.excluded_incomplete_causal_records,
        excluded_incomplete_causal_sha256: &evidence.excluded_incomplete_causal_sha256,
        excluded_incomplete_causal_sample: &evidence.excluded_incomplete_causal_sample,
        resource_cause_record_count: evidence.resource_cause_record_count,
        resource_cause_record_sha256: &evidence.resource_cause_record_sha256,
        resource_cause_missing_records: evidence.resource_cause_missing_records,
        resource_cause_missing_sha256: &evidence.resource_cause_missing_sha256,
        causal_registry_sha256: &evidence.causal_registry_sha256,
        causal_registry_catalog_sha256: &evidence.causal_registry_catalog_sha256,
        causal_registry_serving_slots: &evidence.causal_registry_serving_slots,
        causal_registry_serving_slots_sha256: &evidence.causal_registry_serving_slots_sha256,
        held_out_count: evidence.held_out_count,
        held_out_sha256: &evidence.held_out_sha256,
        guard_profile_sha256: &evidence.guard_profile_sha256,
        goodhart_passed: evidence.goodhart.passed,
        mistakes_passed: evidence.mistakes.passed,
        mistake_count: evidence.mistake_count,
        predictor: &evidence.predictor,
        predictor_slots: &evidence.predictor_slots,
        predictor_sha256: &evidence.predictor_sha256,
        predictor_artifact_blob_id: &evidence.predictor_artifact_blob_id,
        predictor_artifact_blake3: &evidence.predictor_artifact_blake3,
        predictor_artifact_total_bytes: evidence.predictor_artifact_total_bytes,
        predictor_artifact_manifest_seq: evidence.predictor_artifact_manifest_seq,
    })
    .map_err(|error| validation_encode_error("ledger payload", &error))
}

fn action_validation_evidence_content_sha256(
    evidence: &SynapseCalyxActionValidationEvidence,
) -> Result<String, SynapseCalyxError> {
    let mut content = evidence.clone();
    content.ledger_seq = 0;
    content.ledger_hash.clear();
    let bytes = serde_json::to_vec(&content)
        .map_err(|error| validation_encode_error("ledger-bound evidence content", &error))?;
    Ok(hex(&Sha256::digest(bytes)))
}

pub fn action_validation_ledger_payload_sha256(
    evidence: &SynapseCalyxActionValidationEvidence,
) -> Result<String, SynapseCalyxError> {
    Ok(hex(&Sha256::digest(action_validation_ledger_payload(
        evidence,
    )?)))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ActionObservation {
    cx_id: CxId,
    created_at: u64,
    action: String,
    outcome: bool,
    /// Dense causes in the exact order of `ACTION_CAUSAL_PREDICTOR_SLOTS`.
    /// Norms are prepared once at corpus load instead of once per comparison.
    causes: Vec<ActionPreparedCause>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ActionPreparedCause {
    values: Vec<f32>,
    inverse_norm: f64,
}

#[derive(Clone, Debug)]
struct TypedActionPredictionEvidence {
    outcome: bool,
    failed_score: f64,
    succeeded_score: f64,
    support_count: usize,
    separation: f64,
    raw_confidence: f64,
    source_cx_ids: Vec<CxId>,
}

#[derive(Clone, Copy)]
enum ActionPredictionCandidateBoundary {
    StrictlyPriorToQuery,
    CurrentValidatedCorpus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionPredictorArtifact {
    schema_version: u32,
    panel_version: u32,
    predictor: String,
    predictor_slots: Vec<u16>,
    predictor_sha256: String,
    causal_registry_sha256: String,
    causal_registry_catalog_sha256: String,
    anchors_cf_last_commit_seq: u64,
    anchors_cf_out_of_band_epoch: u64,
    action_corpus_sha256: String,
    observations: Vec<ActionObservation>,
}

#[derive(Clone, Debug)]
struct ActionPredictorArtifactBinding {
    blob_id: String,
    content_blake3: String,
    total_bytes: u64,
    manifest_seq: u64,
}

/// Production result of the exact predictor validated by chronological replay.
///
/// The append-only Answer ledger is the physical source of truth for this
/// response; all serving contracts and source identities are hash-bound there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxTypedActionPrediction {
    pub schema_version: u32,
    pub query_cx_id: String,
    pub query_action: String,
    pub predicted_outcome: bool,
    pub failed_score: f64,
    pub succeeded_score: f64,
    pub support_count: usize,
    pub separation: f64,
    pub raw_confidence: f64,
    pub confidence: f64,
    pub dpi_confidence_cap: f64,
    pub goodhart_confidence_cap: f64,
    pub guard_confidence_cap: f64,
    pub validation_confidence_cap: f64,
    pub source_cx_ids: Vec<String>,
    pub predictor: String,
    pub predictor_slots: Vec<u16>,
    pub predictor_sha256: String,
    pub panel_version: u32,
    pub panel_content_seq: u64,
    pub action_corpus_sha256: String,
    pub predictor_artifact_blob_id: String,
    pub predictor_artifact_blake3: String,
    pub causal_registry_sha256: String,
    pub causal_registry_catalog_sha256: String,
    pub guard_profile_sha256: String,
    pub guard_serving_sha256: String,
    pub guard_ledger_seq: u64,
    pub guard_ledger_hash: String,
    pub readiness_row_revision_sha256: String,
    pub validation_row_revision_sha256: String,
    pub validation_ledger_seq: u64,
    pub validation_ledger_hash: String,
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct TypedActionPredictionLedgerPayload<'a> {
    tag: &'static str,
    query_cx_id: &'a str,
    query_action: &'a str,
    predicted_outcome: bool,
    failed_score: f64,
    succeeded_score: f64,
    support_count: usize,
    separation: f64,
    raw_confidence: f64,
    confidence: f64,
    dpi_confidence_cap: f64,
    goodhart_confidence_cap: f64,
    guard_confidence_cap: f64,
    validation_confidence_cap: f64,
    source_cx_ids: &'a [String],
    predictor: &'a str,
    predictor_slots: &'a [u16],
    predictor_sha256: &'a str,
    panel_version: u32,
    panel_content_seq: u64,
    action_corpus_sha256: &'a str,
    predictor_artifact_blob_id: &'a str,
    predictor_artifact_blake3: &'a str,
    causal_registry_sha256: &'a str,
    causal_registry_catalog_sha256: &'a str,
    guard_profile_sha256: &'a str,
    guard_serving_sha256: &'a str,
    guard_ledger_seq: u64,
    guard_ledger_hash: &'a str,
    readiness_row_revision_sha256: &'a str,
    validation_row_revision_sha256: &'a str,
    validation_ledger_seq: u64,
    validation_ledger_hash: &'a str,
}

#[derive(Clone, Debug)]
struct ActionObservationCorpus {
    observations: Vec<ActionObservation>,
    panel_content_seq: u64,
    anchors_cf_last_commit_seq: u64,
    anchors_cf_out_of_band_epoch: u64,
    source_reward_record_count: usize,
    excluded_incomplete_causal: Vec<CxId>,
    resource_cause_present: Vec<CxId>,
    resource_cause_missing: Vec<CxId>,
}

#[derive(Clone, Debug)]
pub struct ActionCausalPopulationBinding {
    pub anchors_cf_last_commit_seq: u64,
    pub anchors_cf_out_of_band_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionCausalRegistryBinding {
    pub registry_sha256: String,
    pub row_revision_sha256: String,
    pub catalog_sha256: String,
    pub source_panel_content_seq: u64,
    pub source_anchors_cf_last_commit_seq: u64,
    pub source_anchors_cf_out_of_band_epoch: u64,
    /// Exact physical panel roster declared by the authenticated Registry.
    /// This lets the canonical readiness boundary prove that every
    /// non-predictor slot was explicitly withheld from the Assay rather than
    /// silently omitted by an in-process caller.
    pub declared_slot_ids: Vec<u16>,
    pub serving_slots: Vec<u16>,
    pub serving_slots_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_field_names,
    reason = "each binding names the exact hash algorithm and distinguishes row-revision identity from decoded-content identity"
)]
struct ActionGuardGenerationBinding {
    profile_row_revision_sha256: String,
    profile_sha256: String,
    serving_row_revision_sha256: String,
    serving_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActionPredictionPublicationSources {
    anchors_cf_last_commit_seq: u64,
    anchors_cf_out_of_band_epoch: u64,
    registry: ActionCausalRegistryBinding,
    guard: ActionGuardGenerationBinding,
    readiness_row_revision_sha256: String,
    readiness_content_sha256: String,
    readiness_ledger_seq: u64,
    readiness_ledger_hash: String,
    validation_row_revision_sha256: String,
    validation_content_sha256: String,
    validation_ledger_seq: u64,
    validation_ledger_hash: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ActionCausalPredictorContract<'a> {
    tag: &'static str,
    panel_version: u32,
    predictor: &'static str,
    ordered_slots: &'a [u16],
    guard_ordered_slots: &'a [u16],
    registry_catalog_sha256: &'a str,
    rrf_k: u32,
    per_slot_top_k: usize,
    final_top_k: usize,
    minimum_unique_sources: usize,
    minimum_normalized_vote_separation: f64,
    minimum_capped_confidence: f64,
    positive_cosine_support_only: bool,
    exclude_query_identity: bool,
    temporal_candidate_rule: &'static str,
    deterministic_tie_break: &'static str,
    vote: &'static str,
    confidence: &'static str,
    exact_tie_verdict: &'static str,
    missing_cause_policy: &'static str,
    equal_score_slot_policy: &'static str,
    per_slot_cutoff_tie_policy: &'static str,
    final_cutoff_tie_policy: &'static str,
}

pub fn action_causal_predictor_sha256(
    registry_catalog_sha256: &str,
) -> Result<String, SynapseCalyxError> {
    let bytes = serde_json::to_vec(&ActionCausalPredictorContract {
        tag: "synapse-action-causal-predictor-contract-v4",
        panel_version: ACTION_PANEL_VERSION,
        predictor: ACTION_CAUSAL_PREDICTOR,
        ordered_slots: ACTION_CAUSAL_PREDICTOR_SLOTS,
        guard_ordered_slots: ACTION_CAUSAL_GUARD_SLOTS,
        registry_catalog_sha256,
        rrf_k: ACTION_RRF_K,
        per_slot_top_k: MAX_RRF_NEIGHBORS_PER_SLOT,
        final_top_k: ACTION_FINAL_NEIGHBORS,
        minimum_unique_sources: MIN_ACTION_PREDICTION_SOURCES,
        minimum_normalized_vote_separation: MIN_ACTION_PREDICTION_SEPARATION,
        minimum_capped_confidence: MIN_ACTION_PREDICTION_CONFIDENCE,
        positive_cosine_support_only: true,
        exclude_query_identity: true,
        temporal_candidate_rule: "candidate.created_at_strictly_before_query.created_at",
        deterministic_tie_break: "cx_id_ascending_for_presentation_only",
        vote: "sum_rrf_score_by_grounded_bool_reward",
        confidence: "winning_share*normalized_vote_separation*support/(support+2), capped by panel sufficiency, held-out guard stability, query-local guard calibration confidence, and mistake-closure consistency",
        exact_tie_verdict: "insufficient",
        missing_cause_policy: "refuse",
        equal_score_slot_policy:
            "skip_only_when_every_candidate_has_the_same_positive_score; otherwise use_equal_competition_rank_within_each_positive_score_tie",
        per_slot_cutoff_tie_policy:
            "include_the_complete_score_tie_group_crossing_top_k; never_select_evidence_by_cx_id",
        final_cutoff_tie_policy:
            "insufficient_if_a_fused_score_tie_crosses_final_top_k; never_select_outcome_evidence_by_cx_id",
    })
    .map_err(|error| validation_encode_error("causal predictor contract", &error))?;
    Ok(hex(&Sha256::digest(bytes)))
}

fn action_predictor_blob_collection() -> Collection {
    Collection {
        name: ACTION_PREDICTOR_BLOB_COLLECTION.to_owned(),
        mode: CollectionMode::Blob,
        schema: None,
        panel: None,
        indexes: Vec::new(),
        dedup: DedupPolicy::Exact,
        temporal: TemporalPolicy::default(),
        retention: RetentionPolicy::Forever,
        txn_policy: TxnPolicy::default(),
        tenant: TenantId::default(),
    }
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
        let corpus = self.action_observations()?;
        let observations = &corpus.observations;
        if observations.len() < MIN_ACTION_RECORDS {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_INSUFFICIENT",
                format!(
                    "action validation found {} complete-cause records from {} grounded reward records ({} historical records lacked writer-sealed admission context); at least {MIN_ACTION_RECORDS} complete-cause records are required",
                    observations.len(),
                    corpus.source_reward_record_count,
                    corpus.excluded_incomplete_causal.len()
                ),
                "collect real terminal action outcomes written by the current action publisher before validating autonomy; historical missing causes are never imputed",
            ));
        }
        let causal_registry = self.current_action_causal_registry_binding()?;
        if causal_registry.source_panel_content_seq != corpus.panel_content_seq {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_STALE",
                format!(
                    "causal Registry source panel watermark {} != validation corpus watermark {}",
                    causal_registry.source_panel_content_seq, corpus.panel_content_seq
                ),
                "run view_registry_measure explicitly on the current action panel, then rerun oracle_validate",
            ));
        }
        if causal_registry.source_anchors_cf_last_commit_seq != corpus.anchors_cf_last_commit_seq
            || causal_registry.source_anchors_cf_out_of_band_epoch
                != corpus.anchors_cf_out_of_band_epoch
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_ANCHORS_STALE",
                format!(
                    "causal Registry Anchors frontier=({},{}) != validation corpus frontier=({},{})",
                    causal_registry.source_anchors_cf_last_commit_seq,
                    causal_registry.source_anchors_cf_out_of_band_epoch,
                    corpus.anchors_cf_last_commit_seq,
                    corpus.anchors_cf_out_of_band_epoch,
                ),
                "run view_registry_measure explicitly after the latest grounded Reward mutation, then rerun oracle_validate",
            ));
        }
        let held_out_count =
            (observations.len() / 5).clamp(MIN_HELD_OUT_RECORDS, MAX_HELD_OUT_RECORDS);
        let split = observations.len() - held_out_count;
        let (training, held_out) = observations.split_at(split);
        let corpus_hash = action_corpus_hash(observations);
        let held_out_hash = action_corpus_hash(held_out);
        let excluded_incomplete_causal_sha256 =
            cx_id_population_hash(&corpus.excluded_incomplete_causal);
        let resource_cause_record_sha256 =
            resource_population_hash(b"present", &corpus.resource_cause_present);
        let resource_cause_missing_sha256 =
            resource_population_hash(b"missing", &corpus.resource_cause_missing);
        let excluded_incomplete_causal_sample = corpus
            .excluded_incomplete_causal
            .iter()
            .take(MAX_EXCLUDED_DIAGNOSTIC_SAMPLE)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let (goodhart, guard_training_successes, guard_held_out_successes, guard_profile_sha256) =
            self.action_goodhart_report(panel_version, observations)?;
        let (mistakes, regression_evaluated, mistake_count) =
            action_mistake_report(training, held_out, observations)?;
        let predictor_sha256 = action_causal_predictor_sha256(&causal_registry.catalog_sha256)?;
        let predictor_artifact = ActionPredictorArtifact {
            schema_version: ACTION_PREDICTOR_ARTIFACT_SCHEMA_VERSION,
            panel_version,
            predictor: ACTION_CAUSAL_PREDICTOR.to_owned(),
            predictor_slots: ACTION_CAUSAL_PREDICTOR_SLOTS.to_vec(),
            predictor_sha256: predictor_sha256.clone(),
            causal_registry_sha256: causal_registry.registry_sha256.clone(),
            causal_registry_catalog_sha256: causal_registry.catalog_sha256.clone(),
            anchors_cf_last_commit_seq: corpus.anchors_cf_last_commit_seq,
            anchors_cf_out_of_band_epoch: corpus.anchors_cf_out_of_band_epoch,
            action_corpus_sha256: corpus_hash.clone(),
            observations: observations.clone(),
        };
        let predictor_artifact_binding =
            self.publish_action_predictor_artifact(&predictor_artifact)?;
        let draft = SynapseCalyxActionValidationEvidence {
            schema_version: ACTION_VALIDATION_SCHEMA_VERSION,
            domain: ACTION_DOMAIN.to_owned(),
            panel_version,
            measured_at_seq,
            panel_content_seq: corpus.panel_content_seq,
            anchors_cf_last_commit_seq: corpus.anchors_cf_last_commit_seq,
            anchors_cf_out_of_band_epoch: corpus.anchors_cf_out_of_band_epoch,
            causal_population_contract: ACTION_CAUSAL_POPULATION_CONTRACT.to_owned(),
            source_reward_record_count: corpus.source_reward_record_count,
            action_record_count: observations.len(),
            action_corpus_sha256: corpus_hash,
            excluded_incomplete_causal_records: corpus.excluded_incomplete_causal.len(),
            excluded_incomplete_causal_sha256,
            excluded_incomplete_causal_sample,
            resource_cause_record_count: corpus.resource_cause_present.len(),
            resource_cause_record_sha256,
            resource_cause_missing_records: corpus.resource_cause_missing.len(),
            resource_cause_missing_sha256,
            causal_registry_sha256: causal_registry.registry_sha256.clone(),
            causal_registry_catalog_sha256: causal_registry.catalog_sha256.clone(),
            causal_registry_serving_slots: causal_registry.serving_slots.clone(),
            causal_registry_serving_slots_sha256: causal_registry.serving_slots_sha256,
            held_out_count,
            held_out_sha256: held_out_hash,
            guard_training_successes,
            guard_held_out_successes,
            guard_profile_sha256,
            regression_evaluated,
            mistake_count,
            predictor: ACTION_CAUSAL_PREDICTOR.to_owned(),
            predictor_slots: ACTION_CAUSAL_PREDICTOR_SLOTS.to_vec(),
            predictor_sha256,
            predictor_artifact_blob_id: predictor_artifact_binding.blob_id,
            predictor_artifact_blake3: predictor_artifact_binding.content_blake3,
            predictor_artifact_total_bytes: predictor_artifact_binding.total_bytes,
            predictor_artifact_manifest_seq: predictor_artifact_binding.manifest_seq,
            goodhart,
            mistakes,
            ledger_seq: 0,
            ledger_hash: String::new(),
        };
        let payload = action_validation_ledger_payload(&draft)?;
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
        let expected_predictor_sha256 =
            action_causal_predictor_sha256(&stored.evidence.causal_registry_catalog_sha256)?;
        if stored.evidence.schema_version != ACTION_VALIDATION_SCHEMA_VERSION
            || stored.evidence.domain != ACTION_DOMAIN
            || stored.evidence.panel_version != ACTION_PANEL_VERSION
            || stored.evidence.predictor != ACTION_CAUSAL_PREDICTOR
            || stored.evidence.predictor_slots.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS
            || stored.evidence.predictor_sha256 != expected_predictor_sha256
            || stored.evidence.predictor_artifact_manifest_seq == 0
            || stored.evidence.predictor_artifact_manifest_seq >= stored.evidence.ledger_seq
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CONTRACT_MISMATCH",
                format!(
                    "validation schema={} domain={} panel={} predictor={} slots={:?} predictor_sha256={} expected_sha256={} artifact_manifest_seq={} ledger_seq={}",
                    stored.evidence.schema_version,
                    stored.evidence.domain,
                    stored.evidence.panel_version,
                    stored.evidence.predictor,
                    stored.evidence.predictor_slots,
                    stored.evidence.predictor_sha256,
                    expected_predictor_sha256,
                    stored.evidence.predictor_artifact_manifest_seq,
                    stored.evidence.ledger_seq,
                ),
                "preserve the mismatched row as historical evidence and rerun oracle_validate for the exact current predictor contract",
            ));
        }
        let expected_payload_sha256 = action_validation_ledger_payload_sha256(&stored.evidence)?;
        let ledger_entry = self.read_ledger_entry(stored.evidence.ledger_seq)?;
        let ledger_bound = ledger_entry.present
            && ledger_entry.kind.as_deref() == Some(EntryKind::Anneal.as_str())
            && ledger_entry.entry_hash.as_deref() == Some(stored.evidence.ledger_hash.as_str())
            && ledger_entry.payload_sha256.as_deref() == Some(expected_payload_sha256.as_str())
            && ledger_entry.self_verifies == Some(true);
        if !ledger_bound {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_LEDGER_UNBOUND",
                format!(
                    "action validation references ledger seq={} hash={}, but physical readback has present={} kind={:?} hash={:?} payload_sha256={:?} expected_payload_sha256={} self_verifies={:?}",
                    stored.evidence.ledger_seq,
                    stored.evidence.ledger_hash,
                    ledger_entry.present,
                    ledger_entry.kind,
                    ledger_entry.entry_hash,
                    ledger_entry.payload_sha256,
                    expected_payload_sha256,
                    ledger_entry.self_verifies,
                ),
                "quarantine the unbound AnnealReport row and rerun oracle_validate through the atomic validation+ledger writer",
            ));
        }
        let _ = self.verify_action_predictor_artifact_manifest(&stored.evidence)?;
        Ok(Some((stored.evidence, row_revision_sha256)))
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the source-binding seam is fallible by contract even though the current CF signal probe is infallible"
    )]
    pub(crate) fn current_action_corpus_binding(
        &self,
    ) -> Result<ActionCausalPopulationBinding, SynapseCalyxError> {
        let (anchors_cf_last_commit_seq, anchors_cf_out_of_band_epoch) =
            self.cf_change_signal(ColumnFamily::Anchors);
        Ok(ActionCausalPopulationBinding {
            anchors_cf_last_commit_seq,
            anchors_cf_out_of_band_epoch,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one authenticated read validates the Registry frontier, catalog, physical availability, estimator roster, and parked resource contract without splitting the evidence boundary"
    )]
    pub(crate) fn current_action_causal_registry_binding(
        &self,
    ) -> Result<ActionCausalRegistryBinding, SynapseCalyxError> {
        let scope = SynapseCalyxCausalViewRegistryScope {
            panel_version: ACTION_PANEL_VERSION,
            corpus_shard: ACTION_DOMAIN.to_owned(),
            anchor_kind: "reward".to_owned(),
        };
        let readback = self.read_causal_view_registry(&scope)?.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_ABSENT",
                "the canonical action causal-view Registry row is absent",
                "measure and persist the canonical panel=2260001 corpus_shard=synapse.action anchor_kind=reward causal-view registry before oracle_validate",
            )
        })?;
        let live_anchors_frontier = self.cf_change_signal(ColumnFamily::Anchors);
        let registry_anchors_frontier = (
            readback.registry.source_anchors_cf_last_commit_seq,
            readback.registry.source_anchors_cf_out_of_band_epoch,
        );
        if registry_anchors_frontier != live_anchors_frontier {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_ANCHORS_STALE",
                format!(
                    "causal Registry Anchors frontier={registry_anchors_frontier:?} != live frontier={live_anchors_frontier:?}"
                ),
                "run view_registry_measure explicitly after the latest grounded outcome mutation; a Registry measured from an older action/reward population is never served",
            ));
        }
        let physically_available = readback
            .registry
            .evidence
            .physically_available_slots
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let estimable = readback
            .registry
            .evidence
            .estimable_slots
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut serving_slots = Vec::new();
        for view in &readback.registry.catalog {
            if !view.producing
                || view.lifecycle != SynapseCalyxCausalViewLifecycle::ServingCodeFrozen
            {
                continue;
            }
            let slot = view.slot.ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_INVALID",
                    format!(
                        "serving producing causal view {} has no physical slot",
                        view.view_id
                    ),
                    "repair and republish the immutable causal-view Registry catalog",
                )
            })?;
            if physically_available.contains(&slot) {
                serving_slots.push(slot);
            }
        }
        serving_slots.sort_unstable();
        serving_slots.dedup();
        if serving_slots.as_slice() != ACTION_CAUSAL_REGISTRY_SERVING_SLOTS {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_SERVING_MISMATCH",
                format!(
                    "causal Registry physically available serving producing slots are {serving_slots:?}, expected {ACTION_CAUSAL_REGISTRY_SERVING_SLOTS:?}"
                ),
                "repair or remeasure the current compact-view Registry; missing/ragged/invalid views never enter serving, while valid constants remain physically available",
            ));
        }
        let physically_available_predictor = ACTION_CAUSAL_PREDICTOR_SLOTS
            .iter()
            .copied()
            .filter(|slot| physically_available.contains(slot))
            .collect::<Vec<_>>();
        if physically_available_predictor.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_PREDICTOR_AVAILABILITY_MISMATCH",
                format!(
                    "causal Registry physically available predictor slots are {physically_available_predictor:?}, expected {ACTION_CAUSAL_PREDICTOR_SLOTS:?}"
                ),
                "repair the writer-sealed complete-cause cohort and remeasure the Registry; every frozen predictor cause must be physically finite, rectangular, and present even when constant",
            ));
        }
        let resource_contract_valid = readback.registry.catalog.iter().any(|view| {
            view.producing
                && view.slot == Some(ACTION_RESOURCE_CAUSE_SLOT)
                && view.lifecycle == SynapseCalyxCausalViewLifecycle::ParkedUnderpowered
        }) && !estimable.contains(&ACTION_RESOURCE_CAUSE_SLOT);
        if !resource_contract_valid {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_REGISTRY_RESOURCE_STATE_INVALID",
                "resource slot 136 is not exactly producing+parked_underpowered+excluded_from_estimable_slots",
                "republish the causal-view Registry with resource slot 136 collection-only until a later powered immutable promotion",
            ));
        }
        let serving_slots_sha256 = action_registry_serving_slots_hash(&serving_slots);
        Ok(ActionCausalRegistryBinding {
            registry_sha256: readback.registry_sha256,
            row_revision_sha256: readback.row_revision_sha256,
            catalog_sha256: readback.registry.catalog_sha256,
            source_panel_content_seq: readback.registry.source_panel_content_seq,
            source_anchors_cf_last_commit_seq: readback.registry.source_anchors_cf_last_commit_seq,
            source_anchors_cf_out_of_band_epoch: readback
                .registry
                .source_anchors_cf_out_of_band_epoch,
            declared_slot_ids: readback.registry.evidence.declared_slot_ids,
            serving_slots,
            serving_slots_sha256,
        })
    }

    fn publish_action_predictor_artifact(
        &self,
        artifact: &ActionPredictorArtifact,
    ) -> Result<ActionPredictorArtifactBinding, SynapseCalyxError> {
        let bytes = encode_action_predictor_artifact(artifact)?;
        let layer = BlobLayer::new(&self.vault);
        let collection = action_predictor_blob_collection();
        let put = layer
            .blob_put_content_addressed(&collection, &bytes)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "persist content-addressed action predictor artifact",
                    &error,
                )
            })?;
        let readback = layer
            .blob_read(&collection, put.blob_id)
            .map_err(|error| {
                SynapseCalyxError::from_calyx(
                    "read back content-addressed action predictor artifact",
                    &error,
                )
            })?
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_READBACK_MISSING",
                    format!(
                        "action predictor Blob manifest disappeared after commit seq {}",
                        put.seq
                    ),
                    "preserve the vault and inspect the Blob CF manifest/chunks before retrying validation",
                )
            })?;
        if readback.manifest != put.manifest || readback.data != bytes {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_READBACK_MISMATCH",
                format!(
                    "action predictor Blob readback differs after commit: expected_bytes={} actual_bytes={} expected_manifest={:?} actual_manifest={:?}",
                    bytes.len(),
                    readback.data.len(),
                    put.manifest,
                    readback.manifest,
                ),
                "preserve the vault and inspect the immutable Blob rows; validation never binds an unverified artifact",
            ));
        }
        let decoded = decode_action_predictor_artifact(&readback.data)?;
        if decoded != *artifact {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_DECODE_MISMATCH",
                "decoded predictor artifact differs from the exact validated corpus",
                "preserve the Blob rows and inspect the bincode format/runtime before retrying validation",
            ));
        }
        Ok(ActionPredictorArtifactBinding {
            blob_id: hex(put.blob_id.as_bytes()),
            content_blake3: hex(&put.manifest.content_hash),
            total_bytes: put.manifest.total_bytes,
            manifest_seq: put.seq,
        })
    }

    fn verify_action_predictor_artifact_manifest(
        &self,
        evidence: &SynapseCalyxActionValidationEvidence,
    ) -> Result<BlobId, SynapseCalyxError> {
        let blob_id = parse_action_predictor_blob_id(&evidence.predictor_artifact_blob_id)?;
        let manifest = BlobLayer::new(&self.vault)
            .blob_manifest(&action_predictor_blob_collection(), blob_id)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read action predictor Blob manifest", &error)
            })?
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_MISSING",
                    format!(
                        "validation references absent predictor Blob {}",
                        evidence.predictor_artifact_blob_id
                    ),
                    "restore the immutable Blob chunks/manifest or rerun oracle_validate to publish a new lowered artifact",
                )
            })?;
        let manifest_blake3 = hex(&manifest.content_hash);
        if manifest.total_bytes != evidence.predictor_artifact_total_bytes
            || manifest_blake3 != evidence.predictor_artifact_blake3
            || manifest.total_bytes == 0
            || manifest.total_bytes > ACTION_PREDICTOR_ARTIFACT_MAX_BYTES as u64
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_MANIFEST_MISMATCH",
                format!(
                    "predictor Blob manifest bytes={} blake3={} validation bytes={} blake3={}",
                    manifest.total_bytes,
                    manifest_blake3,
                    evidence.predictor_artifact_total_bytes,
                    evidence.predictor_artifact_blake3,
                ),
                "preserve the vault and inspect the content-addressed Blob manifest; serving never follows a moved artifact",
            ));
        }
        Ok(blob_id)
    }

    fn load_action_predictor_artifact(
        &self,
        evidence: &SynapseCalyxActionValidationEvidence,
    ) -> Result<ActionPredictorArtifact, SynapseCalyxError> {
        let blob_id = self.verify_action_predictor_artifact_manifest(evidence)?;
        let readback = BlobLayer::new(&self.vault)
            .blob_read(&action_predictor_blob_collection(), blob_id)
            .map_err(|error| {
                SynapseCalyxError::from_calyx("read action predictor Blob payload", &error)
            })?
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_MISSING",
                    "predictor Blob disappeared between manifest and payload reads",
                    "preserve the vault and inspect concurrent retention/repair activity before retrying",
                )
            })?;
        decode_action_predictor_artifact(&readback.data)
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

    fn current_action_guard_generation_binding(
        &self,
    ) -> Result<ActionGuardGenerationBinding, SynapseCalyxError> {
        let signal_before = self.cf_change_signal(ColumnFamily::Guard);
        let profile = self
            .read_cf_latest_revisioned(
                ColumnFamily::Guard,
                &guard_profile_key(ACTION_PANEL_VERSION),
            )?
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_GUARD_PROFILE_ABSENT",
                    format!("action Guard profile row is absent for panel {ACTION_PANEL_VERSION}"),
                    "recalibrate the exact action Guard generation before serving a prediction",
                )
            })?;
        let serving = self
            .read_cf_latest_revisioned(
                ColumnFamily::Guard,
                &guard_serving_key(ACTION_PANEL_VERSION),
            )?
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_GUARD_SERVING_ABSENT",
                    format!("action Guard serving row is absent for panel {ACTION_PANEL_VERSION}"),
                    "recalibrate the exact action Guard generation before serving a prediction",
                )
            })?;
        let signal_after = self.cf_change_signal(ColumnFamily::Guard);
        if signal_before != signal_after {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_GUARD_GENERATION_MOVED",
                format!(
                    "Guard CF changed while reading the action generation: before={signal_before:?} after={signal_after:?}"
                ),
                "retry only after Guard calibration is quiescent; a mixed profile/serving generation is never bound to an Answer",
            ));
        }
        Ok(ActionGuardGenerationBinding {
            profile_row_revision_sha256: hex(&profile.revision_sha256),
            profile_sha256: hex(&Sha256::digest(&profile.value)),
            serving_row_revision_sha256: hex(&serving.revision_sha256),
            serving_sha256: hex(&Sha256::digest(&serving.value)),
        })
    }

    fn verify_action_prediction_publication_sources(
        &self,
        expected: &ActionPredictionPublicationSources,
        phase: &str,
    ) -> Result<(), SynapseCalyxError> {
        let expected_anchors = (
            expected.anchors_cf_last_commit_seq,
            expected.anchors_cf_out_of_band_epoch,
        );
        let anchors_before = self.cf_change_signal(ColumnFamily::Anchors);
        if anchors_before != expected_anchors {
            return Err(prediction_publication_stale(
                phase,
                "Anchors frontier",
                format!("{expected_anchors:?}"),
                format!("{anchors_before:?}"),
            ));
        }

        let registry = self.current_action_causal_registry_binding()?;
        if registry != expected.registry {
            return Err(prediction_publication_stale(
                phase,
                "causal Registry row/content",
                format!("{:?}", expected.registry),
                format!("{registry:?}"),
            ));
        }

        let guard = self.current_action_guard_generation_binding()?;
        if guard != expected.guard {
            return Err(prediction_publication_stale(
                phase,
                "Guard profile/serving rows",
                format!("{:?}", expected.guard),
                format!("{guard:?}"),
            ));
        }

        let readiness = self.read_action_readiness()?.ok_or_else(|| {
            prediction_publication_stale(
                phase,
                "readiness row",
                expected.readiness_row_revision_sha256.clone(),
                "absent".to_owned(),
            )
        })?;
        let readiness_binding = (
            readiness.row_revision_sha256.as_str(),
            readiness.content_sha256.as_str(),
            readiness.ledger_seq,
            readiness.ledger_hash.as_str(),
        );
        let expected_readiness_binding = (
            expected.readiness_row_revision_sha256.as_str(),
            expected.readiness_content_sha256.as_str(),
            expected.readiness_ledger_seq,
            expected.readiness_ledger_hash.as_str(),
        );
        if readiness_binding != expected_readiness_binding {
            return Err(prediction_publication_stale(
                phase,
                "readiness row/content/ledger binding",
                format!("{expected_readiness_binding:?}"),
                format!("{readiness_binding:?}"),
            ));
        }
        self.ensure_action_readiness_sources_current(&readiness)?;

        let (validation, validation_row_revision_sha256) =
            self.read_action_validation_revisioned()?.ok_or_else(|| {
                prediction_publication_stale(
                    phase,
                    "validation row",
                    expected.validation_row_revision_sha256.clone(),
                    "absent".to_owned(),
                )
            })?;
        let validation_content_sha256 = action_validation_evidence_content_sha256(&validation)?;
        let validation_binding = (
            validation_row_revision_sha256.as_str(),
            validation_content_sha256.as_str(),
            validation.ledger_seq,
            validation.ledger_hash.as_str(),
        );
        let expected_validation_binding = (
            expected.validation_row_revision_sha256.as_str(),
            expected.validation_content_sha256.as_str(),
            expected.validation_ledger_seq,
            expected.validation_ledger_hash.as_str(),
        );
        if validation_binding != expected_validation_binding {
            return Err(prediction_publication_stale(
                phase,
                "validation row/content/ledger binding",
                format!("{expected_validation_binding:?}"),
                format!("{validation_binding:?}"),
            ));
        }

        let anchors_after = self.cf_change_signal(ColumnFamily::Anchors);
        if anchors_after != expected_anchors {
            return Err(prediction_publication_stale(
                phase,
                "Anchors frontier after source readback",
                format!("{expected_anchors:?}"),
                format!("{anchors_after:?}"),
            ));
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one bounded snapshot walk validates eligibility, compact causes, and both hash-bound collection populations"
    )]
    fn action_observations(&self) -> Result<ActionObservationCorpus, SynapseCalyxError> {
        let anchors_signal_before = self.cf_change_signal(ColumnFamily::Anchors);
        let mut observations = Vec::new();
        let mut panel_content_seq = None;
        let mut source_reward_record_count = 0usize;
        let mut excluded_incomplete_causal = Vec::new();
        let mut resource_cause_present = Vec::new();
        let mut resource_cause_missing = Vec::new();
        self.with_panel_read_snapshot(
            ACTION_PANEL_VERSION,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| {
                panel_content_seq = Some(snapshot.derived_content_seq());
                self.walk_panel_base_snapshot(snapshot, ACTION_PANEL_VERSION, |snapshot, _key, value| {
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
            let rewards = base
                .anchors
                .iter()
                .filter(|anchor| anchor.kind == AnchorKind::Reward && anchor.confidence > 0.0)
                .collect::<Vec<_>>();
            if rewards.is_empty() {
                return Ok(SynapseCalyxWalkStep::Continue);
            }
            if rewards.len() != 1 {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_OUTCOME_INVALID",
                    format!("action record {} has {} grounded Reward anchors", base.cx_id, rewards.len()),
                    "repair the action outcome anchors; validation requires exactly one canonical grounded Reward observation",
                ));
            }
            let outcome = match rewards[0].value {
                AnchorValue::Bool(value) => value,
                ref value => {
                    return Err(validation_error(
                        "SYNAPSE_CALYX_ACTION_VALIDATION_OUTCOME_TYPE_INVALID",
                        format!(
                            "action record {} grounded Reward anchor is not Bool: {value:?}",
                            base.cx_id
                        ),
                        "repair the canonical action outcome anchor; non-Bool Reward values are never ignored or coerced",
                    ));
                }
            };
            source_reward_record_count = source_reward_record_count.checked_add(1).ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_CORPUS_LIMIT",
                    "action reward-record count overflowed usize",
                    "preserve the vault and inspect the action corpus cardinality",
                )
            })?;
            if source_reward_record_count > MAX_ACTION_RECORDS {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_CORPUS_LIMIT",
                    format!("action reward corpus exceeds the bounded {MAX_ACTION_RECORDS}-record validation budget"),
                    "add a versioned incremental validation window before enabling autonomy on a larger corpus",
                ));
            }
            let hydrated = self.hydrated_constellation_at_snapshot(base.cx_id, snapshot)?;
            let complete_admission = finite_dense_slot(
                hydrated.slots.get(&SlotId::new(ACTION_COMPLETE_CAUSE_SEAL_SLOT)),
            );
            let resource_prestate = collection_cause_present(
                base.cx_id,
                SlotId::new(ACTION_RESOURCE_CAUSE_SLOT),
                hydrated.slots.get(&SlotId::new(ACTION_RESOURCE_CAUSE_SLOT)),
            )?;
            if !complete_admission && resource_prestate {
                return Err(validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSAL_SEAL_PARTIAL",
                    format!(
                        "action record {} carries resource slot {} without complete admission seal slot {}",
                        base.cx_id, ACTION_RESOURCE_CAUSE_SLOT, ACTION_COMPLETE_CAUSE_SEAL_SLOT
                    ),
                    "repair or quarantine the partially measured action constellation; a resource cause never substitutes for the writer-sealed admission snapshot",
                ));
            }
            if !complete_admission {
                // Historical rows written before admission-facts v1 have no
                // point-in-time validator snapshot. Preserve them as one
                // explicitly named, completely hash-bound excluded population.
                excluded_incomplete_causal.push(base.cx_id);
                return Ok(SynapseCalyxWalkStep::Continue);
            }
            if resource_prestate {
                resource_cause_present.push(base.cx_id);
            } else {
                // Resource state is a newly collected cause. Its absence is
                // hash-bound evidence, not imputed and not a reason to discard
                // the powered admission cohort from the serving predictor.
                resource_cause_missing.push(base.cx_id);
            }
            let mut causes = Vec::with_capacity(ACTION_CAUSAL_PREDICTOR_SLOTS.len());
            for raw_slot in ACTION_CAUSAL_PREDICTOR_SLOTS {
                let slot = SlotId::new(*raw_slot);
                let vector = hydrated.slots.get(&slot).ok_or_else(|| validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_MISSING",
                    format!("complete-cause action record {} lacks required compact predictor slot {raw_slot}", base.cx_id),
                    "repair the current action-panel backfill; complete-cause rows never run with a varying predictor set",
                ))?;
                causes.push(prepare_action_cause(base.cx_id, slot, vector)?);
            }
            observations.push(ActionObservation { cx_id: base.cx_id, created_at: base.created_at, action: action.to_owned(), outcome, causes });
            Ok(SynapseCalyxWalkStep::Continue)
                })
            },
        )?;
        observations.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.cx_id.cmp(&right.cx_id))
        });
        let mut seen = BTreeSet::new();
        if let Some(duplicate) = observations.iter().find(|row| !seen.insert(row.cx_id)) {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_OUTCOME_IDENTITY_CONFLICT",
                format!(
                    "causal corpus contains duplicate observation identity {}",
                    duplicate.cx_id
                ),
                "repair the panel membership/Base rows; one content-addressed observation may occur only once",
            ));
        }
        excluded_incomplete_causal.sort_unstable();
        resource_cause_present.sort_unstable();
        resource_cause_missing.sort_unstable();
        let anchors_signal_after = self.cf_change_signal(ColumnFamily::Anchors);
        if anchors_signal_after != anchors_signal_before {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_ANCHOR_FRONTIER_MOVED",
                format!(
                    "Anchors CF changed during validation scan: before={anchors_signal_before:?} after={anchors_signal_after:?}"
                ),
                "rerun oracle_validate on a stable grounded-outcome frontier; validation never publishes a mixed population",
            ));
        }
        Ok(ActionObservationCorpus {
            observations,
            panel_content_seq: panel_content_seq.ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_SNAPSHOT_MISSING",
                    "action corpus scan completed without a panel content watermark",
                    "preserve the vault and inspect the panel-scoped MVCC snapshot boundary",
                )
            })?,
            anchors_cf_last_commit_seq: anchors_signal_before.0,
            anchors_cf_out_of_band_epoch: anchors_signal_before.1,
            source_reward_record_count,
            excluded_incomplete_causal,
            resource_cause_present,
            resource_cause_missing,
        })
    }

    /// Serves the same typed-slot RRF predictor that chronological validation
    /// replayed.  The query is an exact persisted pre-trigger action
    /// constellation; action-id-only recurrence is deliberately not accepted
    /// as causal conditioning.
    ///
    /// # Errors
    ///
    /// Refuses when readiness is absent/not-ready/stale, any bound Registry,
    /// Guard, validation, or corpus identity moved, the query lacks one frozen
    /// cause, or the predictor has no non-tied grounded neighborhood.
    #[expect(
        clippy::too_many_lines,
        reason = "the serving gate, shared predictor, Answer-ledger commit, and physical readback are one causal publication boundary"
    )]
    pub fn predict_typed_action_outcome(
        &self,
        query_cx_id: CxId,
    ) -> Result<SynapseCalyxTypedActionPrediction, SynapseCalyxError> {
        let readiness = self.read_action_readiness()?.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_READINESS_ABSENT",
                "no persisted action readiness snapshot exists",
                "run oracle_validate, then oracle_readiness, and inspect the physical readiness row before prediction",
            )
        })?;
        crate::readiness::ensure_action_readiness_serving_admitted(&readiness)?;
        self.ensure_action_readiness_sources_current(&readiness)?;
        let admitted = readiness.evidence.as_ref().ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_EVIDENCE_ABSENT",
                "ready snapshot carries no held-out validation evidence",
                "quarantine the malformed readiness row and remeasure from a physical oracle_validate report",
            )
        })?;
        let (validation, validation_row_revision_sha256) =
            self.read_action_validation_revisioned()?.ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_VALIDATION_ABSENT",
                    "the readiness source validation row is absent",
                    "rerun oracle_validate and oracle_readiness before serving a prediction",
                )
            })?;
        if admitted.row_revision_sha256 != validation_row_revision_sha256
            || validation.predictor != ACTION_CAUSAL_PREDICTOR
            || validation.predictor_slots.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS
            || validation.predictor_sha256
                != action_causal_predictor_sha256(&validation.causal_registry_catalog_sha256)?
            || admitted.predictor != validation.predictor
            || admitted.predictor_slots != validation.predictor_slots
            || admitted.predictor_sha256 != validation.predictor_sha256
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_VALIDATION_MOVED",
                format!(
                    "readiness validation revision={} live={} predictor={} slots={:?} predictor_sha256={}",
                    admitted.row_revision_sha256,
                    validation_row_revision_sha256,
                    validation.predictor,
                    validation.predictor_slots,
                    validation.predictor_sha256,
                ),
                "rerun oracle_readiness against the current held-out validation row and exact typed predictor contract",
            ));
        }
        let validation_ledger = self.read_ledger_entry(validation.ledger_seq)?;
        let validation_ts_ms = validation_ledger.ts.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_VALIDATION_LEDGER_UNBOUND",
                "validation ledger readback has no commit timestamp",
                "quarantine the unbound validation row and rerun oracle_validate",
            )
        })?;
        let age_ms = self
            .clock_now_ms()?
            .checked_sub(validation_ts_ms)
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_CLOCK_SKEW",
                    "validation ledger timestamp is ahead of the vault clock",
                    "reconcile the vault clock and rerun oracle_validate",
                )
            })?;
        if age_ms > crate::readiness::ACTION_EVIDENCE_LEASE_MS {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_EVIDENCE_STALE",
                format!(
                    "held-out validation age_ms={age_ms} exceeds lease_ms={}",
                    crate::readiness::ACTION_EVIDENCE_LEASE_MS
                ),
                "rerun oracle_validate and oracle_readiness on current grounded outcomes",
            ));
        }
        let registry = self.current_action_causal_registry_binding()?;
        if registry.registry_sha256 != validation.causal_registry_sha256
            || registry.catalog_sha256 != validation.causal_registry_catalog_sha256
            || registry.serving_slots != validation.causal_registry_serving_slots
            || registry.serving_slots_sha256 != validation.causal_registry_serving_slots_sha256
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_CAUSAL_REGISTRY_MOVED",
                "the current causal-view Registry differs from held-out validation",
                "run view_registry_measure explicitly, then rerun oracle_validate and oracle_readiness",
            ));
        }
        let current_guard = self.current_guard_profile_sha256(ACTION_PANEL_VERSION)?;
        if current_guard.as_deref() != Some(validation.guard_profile_sha256.as_str()) {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_GUARD_MOVED",
                format!(
                    "live guard sha256={:?} validation sha256={}",
                    current_guard, validation.guard_profile_sha256
                ),
                "revalidate and remeasure readiness after any Guard calibration change",
            ));
        }
        let anchor_signal = self.current_action_corpus_binding()?;
        if anchor_signal.anchors_cf_last_commit_seq != validation.anchors_cf_last_commit_seq
            || anchor_signal.anchors_cf_out_of_band_epoch != validation.anchors_cf_out_of_band_epoch
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_CORPUS_MOVED",
                format!(
                    "live Anchors CF signal=({},{}), validation=({},{})",
                    anchor_signal.anchors_cf_last_commit_seq,
                    anchor_signal.anchors_cf_out_of_band_epoch,
                    validation.anchors_cf_last_commit_seq,
                    validation.anchors_cf_out_of_band_epoch,
                ),
                "rerun oracle_validate and oracle_readiness after any grounded outcome mutation; new unanchored intent rows do not invalidate the lowered predictor",
            ));
        }
        let artifact = self.load_action_predictor_artifact(&validation)?;
        let artifact_corpus_sha256 = action_corpus_hash(&artifact.observations);
        if artifact.schema_version != ACTION_PREDICTOR_ARTIFACT_SCHEMA_VERSION
            || artifact.panel_version != ACTION_PANEL_VERSION
            || artifact.predictor != ACTION_CAUSAL_PREDICTOR
            || artifact.predictor_slots.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS
            || artifact.predictor_sha256 != validation.predictor_sha256
            || artifact.causal_registry_sha256 != validation.causal_registry_sha256
            || artifact.causal_registry_catalog_sha256 != validation.causal_registry_catalog_sha256
            || artifact.anchors_cf_last_commit_seq != validation.anchors_cf_last_commit_seq
            || artifact.anchors_cf_out_of_band_epoch != validation.anchors_cf_out_of_band_epoch
            || artifact.observations.len() != validation.action_record_count
            || artifact.action_corpus_sha256 != validation.action_corpus_sha256
            || artifact_corpus_sha256 != validation.action_corpus_sha256
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_ARTIFACT_CONTRACT_MISMATCH",
                format!(
                    "lowered predictor artifact schema={} panel={} predictor={} slots={:?} digest={} registry={} catalog={} anchor_signal=({},{}) records={} declared_corpus={} observed_corpus={}",
                    artifact.schema_version,
                    artifact.panel_version,
                    artifact.predictor,
                    artifact.predictor_slots,
                    artifact.predictor_sha256,
                    artifact.causal_registry_sha256,
                    artifact.causal_registry_catalog_sha256,
                    artifact.anchors_cf_last_commit_seq,
                    artifact.anchors_cf_out_of_band_epoch,
                    artifact.observations.len(),
                    artifact.action_corpus_sha256,
                    artifact_corpus_sha256,
                ),
                "preserve the content-addressed Blob and rerun oracle_validate; serving never rehydrates or repairs a drifted lowered predictor",
            ));
        }
        let query = self.action_prediction_query(query_cx_id)?;
        let guard = self.guard_verify(&SynapseCalyxGuardVerifyParams {
            panel_version: ACTION_PANEL_VERSION,
            query_cx_id: query_cx_id.to_string(),
            high_stakes: true,
        })?;
        if !guard.overall_pass
            || guard.provisional
            || guard.domain != ACTION_DOMAIN
            || guard.calibration_anchor_kind.as_deref() != Some(ACTION_GUARD_ANCHOR_KIND)
            || guard.required_slots.as_slice() != ACTION_CAUSAL_GUARD_SLOTS
            || guard.guard_cf_profile_sha256 != validation.guard_profile_sha256
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_QUERY_OUT_OF_REGION",
                format!(
                    "query-local Guard verdict pass={} provisional={} domain={} anchor={:?} required_slots={:?} profile_sha256={} expected_profile_sha256={} failing_slots={:?}",
                    guard.overall_pass,
                    guard.provisional,
                    guard.domain,
                    guard.calibration_anchor_kind,
                    guard.required_slots,
                    guard.guard_cf_profile_sha256,
                    validation.guard_profile_sha256,
                    guard.failing_slots,
                ),
                "inspect the Guard verdict and collect/adjudicate real in-region outcomes; an out-of-region or differently guarded query never receives a terminal prediction",
            ));
        }
        let guard_generation = self.current_action_guard_generation_binding()?;
        if guard_generation.profile_sha256 != guard.guard_cf_profile_sha256
            || guard_generation.serving_sha256 != guard.guard_cf_serving_sha256
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_GUARD_GENERATION_MOVED",
                format!(
                    "query Guard verdict used profile={} serving={}, but physical Guard rows are profile={} serving={} revisions=({}, {})",
                    guard.guard_cf_profile_sha256,
                    guard.guard_cf_serving_sha256,
                    guard_generation.profile_sha256,
                    guard_generation.serving_sha256,
                    guard_generation.profile_row_revision_sha256,
                    guard_generation.serving_row_revision_sha256,
                ),
                "retry only after Guard calibration is quiescent; an Answer is never published from a verdict whose physical Guard generation moved",
            ));
        }
        let prediction = typed_causal_prediction(
            &query,
            artifact.observations.iter(),
            ActionPredictionCandidateBoundary::StrictlyPriorToQuery,
        )?.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_INSUFFICIENT",
                format!("query {query_cx_id} has no non-tied grounded typed-cause neighborhood"),
                "collect grounded outcomes with comparable pre-trigger causes; the predictor never substitutes action-id recurrence",
            )
        })?;
        let panel_sufficiency = readiness
            .report
            .tiers
            .iter()
            .find(|tier| tier.tier == calyx_oracle::Tier::PanelSufficient)
            .ok_or_else(|| validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_SUFFICIENCY_EVIDENCE_MISSING",
                "authenticated readiness has no PanelSufficient tier",
                "quarantine the malformed readiness snapshot and remeasure all six readiness tiers",
            ))?;
        let measured_bits = f64::from(panel_sufficiency.measured_value);
        let entropy_bits = f64::from(panel_sufficiency.threshold);
        if !measured_bits.is_finite()
            || !entropy_bits.is_finite()
            || measured_bits < 0.0
            || entropy_bits <= 0.0
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_SUFFICIENCY_EVIDENCE_INVALID",
                format!(
                    "panel sufficiency measured_bits={measured_bits} outcome_entropy_bits={entropy_bits}"
                ),
                "remeasure grounded panel sufficiency; zero-entropy, negative, or non-finite evidence cannot authorize a prediction",
            ));
        }
        let dpi_confidence_cap = (measured_bits / entropy_bits).clamp(0.0, 1.0);
        let goodhart_confidence_cap = admitted.goodhart_in_region_frac.ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_GOODHART_CAP_MISSING",
                "authenticated readiness has no held-out in-region fraction",
                "rerun oracle_validate and readiness with a complete held-out Guard cohort",
            )
        })?;
        let guard_confidence_cap =
            guard.calibration_confidence.map(f64::from).ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_GUARD_CONFIDENCE_MISSING",
                    "the query-local calibrated Guard verdict carries no confidence bound",
                    "recalibrate the high-stakes Guard with finite confidence evidence",
                )
            })?;
        let evaluated = admitted
            .mistake_regression_evaluated
            .to_f64()
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_VALIDATION_COUNT_OUT_OF_RANGE",
                    "the mistake-regression population cannot be represented as f64",
                    "reduce the bounded validation population and remeasure readiness",
                )
            })?;
        let mistakes = admitted.mistake_regression_count.to_f64().ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_VALIDATION_COUNT_OUT_OF_RANGE",
                "the mistake-regression count cannot be represented as f64",
                "reduce the bounded validation population and remeasure readiness",
            )
        })?;
        let validation_confidence_cap = if evaluated > 0.0 {
            (1.0 - (mistakes / evaluated)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let caps = [
            dpi_confidence_cap,
            goodhart_confidence_cap,
            guard_confidence_cap,
            validation_confidence_cap,
        ];
        if caps
            .iter()
            .any(|cap| !cap.is_finite() || !(0.0..=1.0).contains(cap))
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_CONFIDENCE_CAP_INVALID",
                format!("prediction confidence caps are invalid: {caps:?}"),
                "remeasure the sufficiency, Guard, Goodhart, and regression evidence; confidence is never silently clamped from malformed evidence",
            ));
        }
        let confidence = caps.into_iter().fold(prediction.raw_confidence, f64::min);
        if confidence < MIN_ACTION_PREDICTION_CONFIDENCE {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_CONFIDENCE_INSUFFICIENT",
                format!(
                    "capped confidence {confidence} is below frozen floor {MIN_ACTION_PREDICTION_CONFIDENCE}; raw={} dpi_cap={} goodhart_cap={} guard_cap={} validation_cap={} support={} separation={}",
                    prediction.raw_confidence,
                    dpi_confidence_cap,
                    goodhart_confidence_cap,
                    guard_confidence_cap,
                    validation_confidence_cap,
                    prediction.support_count,
                    prediction.separation,
                ),
                "collect stronger comparable grounded outcomes or improve the pre-trigger causal panel; a weak neighborhood returns Insufficient",
            ));
        }
        let source_cx_ids = prediction
            .source_cx_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let query_cx_id_text = query_cx_id.to_string();
        let publication_sources = ActionPredictionPublicationSources {
            anchors_cf_last_commit_seq: validation.anchors_cf_last_commit_seq,
            anchors_cf_out_of_band_epoch: validation.anchors_cf_out_of_band_epoch,
            registry: registry.clone(),
            guard: guard_generation,
            readiness_row_revision_sha256: readiness.row_revision_sha256.clone(),
            readiness_content_sha256: readiness.content_sha256.clone(),
            readiness_ledger_seq: readiness.ledger_seq,
            readiness_ledger_hash: readiness.ledger_hash.clone(),
            validation_row_revision_sha256: validation_row_revision_sha256.clone(),
            validation_content_sha256: action_validation_evidence_content_sha256(&validation)?,
            validation_ledger_seq: validation.ledger_seq,
            validation_ledger_hash: validation.ledger_hash.clone(),
        };
        self.verify_action_prediction_publication_sources(
            &publication_sources,
            "before Answer payload assembly",
        )?;
        let ledger_payload = TypedActionPredictionLedgerPayload {
            tag: "synapse-typed-action-prediction-v2",
            query_cx_id: &query_cx_id_text,
            query_action: &query.action,
            predicted_outcome: prediction.outcome,
            failed_score: prediction.failed_score,
            succeeded_score: prediction.succeeded_score,
            support_count: prediction.support_count,
            separation: prediction.separation,
            raw_confidence: prediction.raw_confidence,
            confidence,
            dpi_confidence_cap,
            goodhart_confidence_cap,
            guard_confidence_cap,
            validation_confidence_cap,
            source_cx_ids: &source_cx_ids,
            predictor: ACTION_CAUSAL_PREDICTOR,
            predictor_slots: ACTION_CAUSAL_PREDICTOR_SLOTS,
            predictor_sha256: &validation.predictor_sha256,
            panel_version: ACTION_PANEL_VERSION,
            panel_content_seq: validation.panel_content_seq,
            action_corpus_sha256: &validation.action_corpus_sha256,
            predictor_artifact_blob_id: &validation.predictor_artifact_blob_id,
            predictor_artifact_blake3: &validation.predictor_artifact_blake3,
            causal_registry_sha256: &registry.registry_sha256,
            causal_registry_catalog_sha256: &registry.catalog_sha256,
            guard_profile_sha256: &guard.guard_cf_profile_sha256,
            guard_serving_sha256: &guard.guard_cf_serving_sha256,
            guard_ledger_seq: guard.ledger_seq,
            guard_ledger_hash: &guard.ledger_hash,
            readiness_row_revision_sha256: &readiness.row_revision_sha256,
            validation_row_revision_sha256: &validation_row_revision_sha256,
            validation_ledger_seq: validation.ledger_seq,
            validation_ledger_hash: &validation.ledger_hash,
        };
        let ledger_bytes = serde_json::to_vec(&ledger_payload)
            .map_err(|error| validation_encode_error("typed prediction ledger payload", &error))?;
        let expected_payload_sha256 = hex(&Sha256::digest(&ledger_bytes));
        let mut publication_guard_error: Option<SynapseCalyxError> = None;
        let append_result = self.vault.append_ledger_entry_with_rows(
            EntryKind::Answer,
            SubjectId::Query(query_cx_id.as_bytes().to_vec()),
            ledger_bytes,
            ActorId::Service("synapse-typed-action-predictor".to_owned()),
            |_ledger_ref| {
                // This closure runs while Aster holds the process and
                // cross-process durable commit boundary. Every mutable
                // authority used to form the Answer is re-read here after
                // ledger staging and before the append becomes visible.
                // Returning an error leaves both the Ledger and every
                // caller-owned row uncommitted.
                if let Err(error) = self.verify_action_prediction_publication_sources(
                    &publication_sources,
                    "at the atomic Answer publication boundary",
                ) {
                    let message = error.to_string();
                    publication_guard_error = Some(error);
                    return Err(calyx_core::CalyxError::ledger_group_commit_failed(message));
                }
                Ok(Vec::new())
            },
        );
        if let Some(error) = publication_guard_error {
            if append_result.is_ok() {
                return Err(validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_PUBLICATION_GUARD_BYPASSED",
                    "Answer publication reported success after its in-lock source guard refused",
                    "preserve the Ledger and repair the atomic Answer publication boundary before trusting any prediction",
                ));
            }
            return Err(error);
        }
        let ledger_ref = append_result.map_err(|error| {
            SynapseCalyxError::from_calyx("append typed action prediction ledger", &error)
        })?;
        let ledger_hash = hex(&ledger_ref.hash);
        let ledger_readback = self.read_ledger_entry(ledger_ref.seq)?;
        if !ledger_readback.present
            || ledger_readback.kind.as_deref() != Some(EntryKind::Answer.as_str())
            || ledger_readback.entry_hash.as_deref() != Some(ledger_hash.as_str())
            || ledger_readback.payload_sha256.as_deref() != Some(expected_payload_sha256.as_str())
            || ledger_readback.self_verifies != Some(true)
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_LEDGER_READBACK_FAILED",
                format!(
                    "prediction ledger seq={} hash={} readback present={} kind={:?} hash={:?} payload={:?} self_verifies={:?}",
                    ledger_ref.seq,
                    ledger_hash,
                    ledger_readback.present,
                    ledger_readback.kind,
                    ledger_readback.entry_hash,
                    ledger_readback.payload_sha256,
                    ledger_readback.self_verifies,
                ),
                "preserve the vault and inspect the Answer ledger append before retrying",
            ));
        }
        self.verify_action_prediction_publication_sources(
            &publication_sources,
            "after Answer ledger physical readback",
        )?;
        Ok(SynapseCalyxTypedActionPrediction {
            schema_version: 2,
            query_cx_id: query_cx_id_text,
            query_action: query.action,
            predicted_outcome: prediction.outcome,
            failed_score: prediction.failed_score,
            succeeded_score: prediction.succeeded_score,
            support_count: prediction.support_count,
            separation: prediction.separation,
            raw_confidence: prediction.raw_confidence,
            confidence,
            dpi_confidence_cap,
            goodhart_confidence_cap,
            guard_confidence_cap,
            validation_confidence_cap,
            source_cx_ids,
            predictor: ACTION_CAUSAL_PREDICTOR.to_owned(),
            predictor_slots: ACTION_CAUSAL_PREDICTOR_SLOTS.to_vec(),
            predictor_sha256: validation.predictor_sha256,
            panel_version: ACTION_PANEL_VERSION,
            panel_content_seq: validation.panel_content_seq,
            action_corpus_sha256: validation.action_corpus_sha256,
            predictor_artifact_blob_id: validation.predictor_artifact_blob_id,
            predictor_artifact_blake3: validation.predictor_artifact_blake3,
            causal_registry_sha256: registry.registry_sha256,
            causal_registry_catalog_sha256: registry.catalog_sha256,
            guard_profile_sha256: guard.guard_cf_profile_sha256,
            guard_serving_sha256: guard.guard_cf_serving_sha256,
            guard_ledger_seq: guard.ledger_seq,
            guard_ledger_hash: guard.ledger_hash,
            readiness_row_revision_sha256: readiness.row_revision_sha256,
            validation_row_revision_sha256,
            validation_ledger_seq: validation.ledger_seq,
            validation_ledger_hash: validation.ledger_hash,
            ledger_seq: ledger_ref.seq,
            ledger_hash,
        })
    }

    fn action_prediction_query(
        &self,
        query_cx_id: CxId,
    ) -> Result<ActionObservation, SynapseCalyxError> {
        let query = self.hydrate_constellation_latest(query_cx_id)?;
        if query.panel_version != ACTION_PANEL_VERSION
            || query.metadata.get("oracle.domain").map(String::as_str) != Some(ACTION_DOMAIN)
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_QUERY_SCOPE_INVALID",
                format!(
                    "query {query_cx_id} has panel={} domain={:?}",
                    query.panel_version,
                    query.metadata.get("oracle.domain")
                ),
                "supply the exact cx_id of a persisted current-generation synapse.action constellation",
            ));
        }
        let row_kind = query.metadata.get("action_row_kind").map(String::as_str);
        let phase = query.metadata.get("action_phase").map(String::as_str);
        let status = query.metadata.get("action_status").map(String::as_str);
        if row_kind != Some("command_audit")
            || phase != Some("intent")
            || status != Some("pending")
            || !query.anchors.is_empty()
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_QUERY_ROLE_INVALID",
                format!(
                    "query {query_cx_id} role row_kind={row_kind:?} phase={phase:?} action_status={:?} anchors={}",
                    status,
                    query.anchors.len(),
                ),
                "supply the exact cx_id of a writer-sealed command_audit phase=intent row with action_status=pending and no grounded outcome; terminal rows are evidence, never prediction queries",
            ));
        }
        if !finite_dense_slot(
            query
                .slots
                .get(&SlotId::new(ACTION_COMPLETE_CAUSE_SEAL_SLOT)),
        ) {
            return Err(validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_QUERY_CAUSES_INCOMPLETE",
                format!(
                    "query {query_cx_id} lacks complete admission seal slot {ACTION_COMPLETE_CAUSE_SEAL_SLOT}"
                ),
                "supply a current action constellation whose pre-trigger admission facts were writer-sealed; missing causes are never imputed",
            ));
        }
        let action = query
            .metadata
            .get("oracle.action")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_TYPED_PREDICTION_QUERY_ACTION_MISSING",
                    format!("query {query_cx_id} has no oracle.action identity"),
                    "repair the source action publication before prediction",
                )
            })?
            .to_owned();
        let mut causes = Vec::with_capacity(ACTION_CAUSAL_PREDICTOR_SLOTS.len());
        for raw_slot in ACTION_CAUSAL_PREDICTOR_SLOTS {
            let slot = SlotId::new(*raw_slot);
            let vector = query.slots.get(&slot).ok_or_else(|| validation_error(
                "SYNAPSE_CALYX_TYPED_PREDICTION_QUERY_CAUSES_INCOMPLETE",
                format!("query {query_cx_id} lacks predictor slot {raw_slot}"),
                "repair the current action-panel measurement; serving never drops a required causal atom",
            ))?;
            causes.push(prepare_action_cause(query_cx_id, slot, vector)?);
        }
        Ok(ActionObservation {
            cx_id: query_cx_id,
            created_at: query.created_at,
            action,
            outcome: false,
            causes,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one held-out measurement must retain its calibrated profile, corpus split, violations, and exact profile revision"
    )]
    fn action_goodhart_report(
        &self,
        panel_version: u32,
        observations: &[ActionObservation],
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
        let required_slots = profile
            .required_slots
            .iter()
            .map(|slot| slot.get())
            .collect::<Vec<_>>();
        if required_slots.as_slice() != ACTION_CAUSAL_GUARD_SLOTS {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_GUARD_ROSTER_MISMATCH",
                format!(
                    "action-panel Ward profile requires slots {required_slots:?}, but the frozen query-local Guard roster is {ACTION_CAUSAL_GUARD_SLOTS:?}"
                ),
                "recalibrate the action guard with exactly the ordered OOD-discriminative Guard roster; structurally constant, daemon-global, audit-only, redundant, and parked views cannot define a conformal query boundary",
            ));
        }
        calyx_ward::validate_high_stakes_profile(&profile, &profile.required_slots).map_err(
            |error| {
                validation_error(
                    error.code(),
                    format!(
                        "action-panel Ward profile has no current high-stakes scoring contract: {error}"
                    ),
                    "recalibrate the action-panel guard so every required slot carries a serving-score parity envelope before oracle_validate",
                )
            },
        )?;
        // Goodhart stability is a conditional question over successful
        // actions: among actions that achieved their outcome, does the frozen
        // trusted region still admit later successes? Splitting the full,
        // failure-dominated stream first lets class prevalence choose the
        // success boundary and can leave every success on one side. Select the
        // outcome cohort first, retain chronological order, then hold out its
        // newest fifth (with the same finite floor/cap). Mistake replay below
        // deliberately retains the full-stream chronological split.
        let successful = observations
            .iter()
            .filter(|row| row.outcome)
            .collect::<Vec<_>>();
        let success_held_out_count = (successful.len() / 5)
            .clamp(MIN_HELD_OUT_RECORDS, MAX_HELD_OUT_RECORDS)
            .min(successful.len());
        let success_split = successful.len() - success_held_out_count;
        let (success_training, held_out_good) = successful.split_at(success_split);
        let training_good = success_training
            .iter()
            .copied()
            .rev()
            .take(MAX_GUARD_TRAINING_RECORDS)
            .collect::<Vec<_>>();
        if training_good.len() < MIN_HELD_OUT_RECORDS || held_out_good.len() < MIN_HELD_OUT_RECORDS
        {
            return Err(validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_GOODHART_INSUFFICIENT",
                format!(
                    "Goodhart success cohort has {} total successes, {} training successes, and {} held-out successes across {} complete-cause action records",
                    successful.len(),
                    training_good.len(),
                    held_out_good.len(),
                    observations.len()
                ),
                "collect at least twenty real complete-cause successful actions so the chronological success cohort has ten on both sides",
            ));
        }
        let mut trusted = Vec::with_capacity(training_good.len());
        for row in training_good {
            trusted.push(self.action_dense_slots(row.cx_id, &profile.required_slots)?);
        }
        let mut accepted = 0usize;
        for row in held_out_good {
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

fn finite_dense_slot(vector: Option<&SlotVector>) -> bool {
    matches!(
        vector,
        Some(SlotVector::Dense { data, .. })
            if !data.is_empty() && data.iter().all(|value| value.is_finite())
    )
}

fn collection_cause_present(
    cx_id: CxId,
    slot: SlotId,
    vector: Option<&SlotVector>,
) -> Result<bool, SynapseCalyxError> {
    match vector {
        None | Some(SlotVector::Absent { .. }) => Ok(false),
        Some(vector @ SlotVector::Dense { .. }) => {
            prepare_action_cause(cx_id, slot, vector)?;
            Ok(true)
        }
        Some(_) => Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_INVALID",
            format!(
                "action record {cx_id} collection-only slot {} stores a non-dense, non-absent vector",
                slot.get()
            ),
            "repair or quarantine the malformed slot row; collection coverage never guesses a vector kind",
        )),
    }
}

fn prepare_action_cause(
    cx_id: CxId,
    slot: SlotId,
    vector: &SlotVector,
) -> Result<ActionPreparedCause, SynapseCalyxError> {
    let SlotVector::Dense { dim, data } = vector else {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_MISSING",
            format!(
                "complete-cause action record {cx_id} required dense predictor slot {} but stored a non-dense vector",
                slot.get()
            ),
            "repair the current action-panel backfill; the frozen predictor accepts only its declared dense slot shapes",
        ));
    };
    let declared_dim = usize::try_from(*dim).map_err(|_| {
        validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_INVALID",
            format!(
                "action record {cx_id} predictor slot {} dimension {dim} cannot be represented as usize",
                slot.get()
            ),
            "repair or quarantine the malformed slot row before validation",
        )
    })?;
    if data.is_empty() || data.len() != declared_dim || data.iter().any(|value| !value.is_finite())
    {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_INVALID",
            format!(
                "action record {cx_id} predictor slot {} has declared_dim={declared_dim}, data_len={}, or a non-finite value",
                slot.get(),
                data.len()
            ),
            "repair or quarantine the malformed current-generation slot row; missing or non-finite causes are never skipped",
        ));
    }
    let norm_squared = data.iter().fold(0.0_f64, |sum, value| {
        f64::from(*value).mul_add(f64::from(*value), sum)
    });
    if !norm_squared.is_finite() || norm_squared <= 0.0 {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_INVALID",
            format!(
                "action record {cx_id} predictor slot {} has non-positive or non-finite L2 norm squared {norm_squared}",
                slot.get()
            ),
            "repair the frozen measurement; a zero-direction cause cannot participate in cosine evidence",
        ));
    }
    let inverse_norm = norm_squared.sqrt().recip();
    if !inverse_norm.is_finite() {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_INVALID",
            format!(
                "action record {cx_id} predictor slot {} produced non-finite inverse norm {inverse_norm}",
                slot.get()
            ),
            "repair the malformed slot vector before validation",
        ));
    }
    Ok(ActionPreparedCause {
        values: data.clone(),
        inverse_norm,
    })
}

fn action_mistake_report(
    training: &[ActionObservation],
    held_out: &[ActionObservation],
    all: &[ActionObservation],
) -> Result<(RegressionReport, usize, usize), SynapseCalyxError> {
    if training.len().checked_add(held_out.len()) != Some(all.len()) {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SPLIT_INVALID",
            "chronological training and held-out slices do not cover the complete action corpus",
            "repair the immutable chronological split before replaying predictions",
        ));
    }
    let mut results = Vec::new();
    let mut evaluated = 0usize;
    for (offset, row) in held_out.iter().enumerate() {
        let prior_end = training.len().checked_add(offset).ok_or_else(|| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_VALIDATION_COUNT_OUT_OF_RANGE",
                "chronological prior boundary overflowed usize",
                "reduce the bounded action validation corpus",
            )
        })?;
        if let Some(old_evidence) = typed_causal_prediction(
            row,
            all[..prior_end].iter(),
            ActionPredictionCandidateBoundary::StrictlyPriorToQuery,
        )? {
            let old = old_evidence.outcome;
            evaluated += 1;
            if old != row.outcome {
                let now = typed_causal_prediction(
                    row,
                    all.iter(),
                    ActionPredictionCandidateBoundary::CurrentValidatedCorpus,
                )?;
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
                            f64::from(u8::from(now.outcome)),
                            if now.outcome == row.outcome { 0.0 } else { 1.0 },
                            now.outcome != row.outcome,
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

#[expect(
    clippy::too_many_lines,
    reason = "the frozen per-slot funnel, fusion, support gate, and confidence derivation form one predictor contract"
)]
fn typed_causal_prediction<'a>(
    query: &ActionObservation,
    candidates: impl IntoIterator<Item = &'a ActionObservation>,
    boundary: ActionPredictionCandidateBoundary,
) -> Result<Option<TypedActionPredictionEvidence>, SynapseCalyxError> {
    let candidates = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.cx_id != query.cx_id
                && match boundary {
                    ActionPredictionCandidateBoundary::StrictlyPriorToQuery => {
                        candidate.created_at < query.created_at
                    }
                    ActionPredictionCandidateBoundary::CurrentValidatedCorpus => true,
                }
        })
        .collect::<Vec<_>>();
    if query.causes.len() != ACTION_CAUSAL_PREDICTOR_SLOTS.len()
        || candidates
            .iter()
            .any(|candidate| candidate.causes.len() != ACTION_CAUSAL_PREDICTOR_SLOTS.len())
    {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_CAUSE_SET_INVALID",
            "typed causal predictor received an observation whose prepared cause count differs from its frozen slot contract",
            "repair the action corpus loader; the predictor never skips a missing cause or compares varying panels",
        ));
    }
    let mut fused = vec![0.0_f64; candidates.len()];
    let mut slot_ranking = Vec::with_capacity(candidates.len());
    for (slot_index, raw_slot) in ACTION_CAUSAL_PREDICTOR_SLOTS.iter().enumerate() {
        let query_vector = &query.causes[slot_index];
        slot_ranking.clear();
        for (candidate_index, candidate) in candidates.iter().enumerate() {
            let score =
                prepared_dense_cosine(*raw_slot, query_vector, &candidate.causes[slot_index])?;
            if score > 0.0 {
                slot_ranking.push((score, candidate_index));
            }
        }
        if slot_ranking.len() == candidates.len()
            && slot_ranking.len() > 1
            && slot_ranking
                .windows(2)
                .all(|pair| pair[0].0.to_bits() == pair[1].0.to_bits())
        {
            // An outcome-independent tied slot has no neighbor-order signal.
            // Ranking its ties by CxId would manufacture deterministic support
            // for low ids and pseudo-replicate a collapsed view. Keep the slot
            // in the authenticated cause roster, but contribute no RRF weight.
            continue;
        }
        if slot_ranking.len() > MAX_RRF_NEIGHBORS_PER_SLOT {
            let _ = slot_ranking
                .select_nth_unstable_by(MAX_RRF_NEIGHBORS_PER_SLOT - 1, |left, right| {
                    compare_action_neighbor_f32(&candidates, left, right)
                });
            let cutoff_score = slot_ranking[MAX_RRF_NEIGHBORS_PER_SLOT - 1].0;
            slot_ranking
                .retain(|(score, _)| score.total_cmp(&cutoff_score) != std::cmp::Ordering::Less);
        }
        slot_ranking
            .sort_unstable_by(|left, right| compare_action_neighbor_f32(&candidates, left, right));
        let mut tie_start = 0;
        while tie_start < slot_ranking.len() {
            let score_bits = slot_ranking[tie_start].0.to_bits();
            let tie_end = slot_ranking[tie_start..]
                .iter()
                .position(|(score, _)| score.to_bits() != score_bits)
                .map_or(slot_ranking.len(), |offset| tie_start + offset);
            let rank = tie_start.to_f64().ok_or_else(|| {
                validation_error(
                    "SYNAPSE_CALYX_ACTION_VALIDATION_RANK_OUT_OF_RANGE",
                    "causal neighbor competition rank cannot be represented as f64",
                    "reduce the bounded action validation corpus",
                )
            })? + 1.0;
            let contribution = 1.0 / (f64::from(ACTION_RRF_K) + rank);
            for &(_, candidate_index) in &slot_ranking[tie_start..tie_end] {
                fused[candidate_index] += contribution;
            }
            tie_start = tie_end;
        }
    }
    let mut ranking = fused
        .into_iter()
        .enumerate()
        .filter(|(_, score)| *score > 0.0)
        .map(|(candidate_index, score)| (score, candidate_index))
        .collect::<Vec<_>>();
    ranking.sort_unstable_by(|left, right| compare_action_neighbor_f64(&candidates, left, right));
    if ranking.len() > ACTION_FINAL_NEIGHBORS {
        let cutoff_score = ranking[ACTION_FINAL_NEIGHBORS - 1].0;
        if ranking[ACTION_FINAL_NEIGHBORS].0.to_bits() == cutoff_score.to_bits() {
            // Selecting an arbitrary subset of equally supported physical
            // outcomes would make CxId influence the prediction. The query is
            // honestly insufficient until other atoms break the fused tie.
            return Ok(None);
        }
        ranking.truncate(ACTION_FINAL_NEIGHBORS);
    }
    let mut failed = 0.0;
    let mut succeeded = 0.0;
    let mut source_cx_ids = Vec::with_capacity(ranking.len());
    for (score, candidate_index) in ranking {
        source_cx_ids.push(candidates[candidate_index].cx_id);
        if candidates[candidate_index].outcome {
            succeeded += score;
        } else {
            failed += score;
        }
    }
    let support_count = source_cx_ids.len();
    let total_score = succeeded + failed;
    if support_count < MIN_ACTION_PREDICTION_SOURCES
        || !total_score.is_finite()
        || total_score <= 0.0
    {
        return Ok(None);
    }
    let separation = (succeeded - failed).abs() / total_score;
    if !separation.is_finite() || separation < MIN_ACTION_PREDICTION_SEPARATION {
        return Ok(None);
    }
    let support = support_count.to_f64().ok_or_else(|| {
        validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SUPPORT_OUT_OF_RANGE",
            format!("prediction support count {support_count} cannot be represented as f64"),
            "reduce the bounded final neighbor count before serving a prediction",
        )
    })?;
    let winning_share = succeeded.max(failed) / total_score;
    let raw_confidence = winning_share * separation * (support / (support + 2.0));
    if !raw_confidence.is_finite() || !(0.0..=1.0).contains(&raw_confidence) {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_CONFIDENCE_INVALID",
            format!(
                "prediction confidence is invalid: winning_share={winning_share} separation={separation} support={support_count} raw_confidence={raw_confidence}"
            ),
            "repair the bounded RRF confidence arithmetic; no outcome is emitted with a non-finite or out-of-range confidence",
        ));
    }
    Ok(Some(TypedActionPredictionEvidence {
        outcome: succeeded > failed,
        failed_score: failed,
        succeeded_score: succeeded,
        support_count,
        separation,
        raw_confidence,
        source_cx_ids,
    }))
}

fn compare_action_neighbor_f32(
    candidates: &[&ActionObservation],
    left: &(f32, usize),
    right: &(f32, usize),
) -> std::cmp::Ordering {
    right
        .0
        .total_cmp(&left.0)
        .then_with(|| candidates[left.1].cx_id.cmp(&candidates[right.1].cx_id))
}

fn compare_action_neighbor_f64(
    candidates: &[&ActionObservation],
    left: &(f64, usize),
    right: &(f64, usize),
) -> std::cmp::Ordering {
    right
        .0
        .total_cmp(&left.0)
        .then_with(|| candidates[left.1].cx_id.cmp(&candidates[right.1].cx_id))
}

fn prepared_dense_cosine(
    slot: u16,
    left: &ActionPreparedCause,
    right: &ActionPreparedCause,
) -> Result<f32, SynapseCalyxError> {
    if left.values.len() != right.values.len() || left.values.is_empty() {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SLOT_DIMENSION_MISMATCH",
            format!(
                "predictor slot {slot} compares dimensions {} and {}",
                left.values.len(),
                right.values.len()
            ),
            "repair or quarantine the current-generation slot rows; the predictor never skips a dimension mismatch",
        ));
    }
    let dot = left
        .values
        .iter()
        .zip(&right.values)
        .fold(0.0_f64, |sum, (left, right)| {
            f64::from(*left).mul_add(f64::from(*right), sum)
        });
    let score = (dot * left.inverse_norm * right.inverse_norm).clamp(-1.0, 1.0);
    if !score.is_finite() {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SIMILARITY_NON_FINITE",
            format!("predictor slot {slot} produced non-finite cosine {score}"),
            "repair the malformed cause vectors; validation never discards a non-finite comparison",
        ));
    }
    score.to_f32().ok_or_else(|| {
        validation_error(
            "SYNAPSE_CALYX_ACTION_VALIDATION_SIMILARITY_OUT_OF_RANGE",
            format!("predictor slot {slot} cosine {score} cannot be represented as f32"),
            "repair the numeric runtime before validating autonomy",
        )
    })
}

fn action_corpus_hash(rows: &[ActionObservation]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-action-validation-corpus-v5-compact-causes");
    hasher.update(u64::try_from(rows.len()).unwrap_or(u64::MAX).to_be_bytes());
    for row in rows {
        hasher.update(row.cx_id.as_bytes());
        hasher.update(row.created_at.to_be_bytes());
        hasher.update(
            u64::try_from(row.action.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        hasher.update(row.action.as_bytes());
        hasher.update([u8::from(row.outcome)]);
        hasher.update(
            u64::try_from(row.causes.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for (slot, cause) in ACTION_CAUSAL_PREDICTOR_SLOTS.iter().zip(&row.causes) {
            hasher.update(slot.to_be_bytes());
            hasher.update(
                u64::try_from(cause.values.len())
                    .unwrap_or(u64::MAX)
                    .to_be_bytes(),
            );
            for value in &cause.values {
                hasher.update(value.to_bits().to_be_bytes());
            }
        }
    }
    hex(&hasher.finalize())
}

fn encode_action_predictor_artifact(
    artifact: &ActionPredictorArtifact,
) -> Result<Vec<u8>, SynapseCalyxError> {
    let bytes = bincode::serde::encode_to_vec(
        artifact,
        bincode::config::standard().with_limit::<ACTION_PREDICTOR_ARTIFACT_MAX_BYTES>(),
    )
    .map_err(|error| {
        validation_error(
            "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_ENCODE_FAILED",
            format!("encode lowered action predictor artifact: {error}"),
            "reduce the bounded validation population or publish a new chunked artifact contract; the artifact is never truncated",
        )
    })?;
    if bytes.is_empty() || bytes.len() > ACTION_PREDICTOR_ARTIFACT_MAX_BYTES {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_SIZE_INVALID",
            format!(
                "encoded predictor artifact length {} is outside 1..={ACTION_PREDICTOR_ARTIFACT_MAX_BYTES}",
                bytes.len()
            ),
            "reduce the bounded validation population or publish a versioned streaming artifact before retrying",
        ));
    }
    Ok(bytes)
}

fn decode_action_predictor_artifact(
    bytes: &[u8],
) -> Result<ActionPredictorArtifact, SynapseCalyxError> {
    if bytes.is_empty() || bytes.len() > ACTION_PREDICTOR_ARTIFACT_MAX_BYTES {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_SIZE_INVALID",
            format!(
                "persisted predictor artifact length {} is outside 1..={ACTION_PREDICTOR_ARTIFACT_MAX_BYTES}",
                bytes.len()
            ),
            "restore or republish the bounded content-addressed predictor artifact",
        ));
    }
    let (artifact, consumed) = bincode::serde::decode_from_slice::<ActionPredictorArtifact, _>(
        bytes,
        bincode::config::standard().with_limit::<ACTION_PREDICTOR_ARTIFACT_MAX_BYTES>(),
    )
    .map_err(|error| {
        validation_error(
            "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_DECODE_FAILED",
            format!("decode lowered action predictor artifact: {error}"),
            "restore or republish the exact supported content-addressed predictor artifact",
        )
    })?;
    if consumed != bytes.len() {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_TRAILING_BYTES",
            format!(
                "predictor artifact decoder consumed {consumed} of {} bytes",
                bytes.len()
            ),
            "preserve the Blob and inspect format drift; serving never ignores trailing bytes",
        ));
    }
    Ok(artifact)
}

fn parse_action_predictor_blob_id(value: &str) -> Result<BlobId, SynapseCalyxError> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(validation_error(
            "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_ID_INVALID",
            format!("predictor Blob id must be exactly 32 hex characters, got {value:?}"),
            "quarantine the malformed validation row and rerun oracle_validate",
        ));
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&value[start..start + 2], 16).map_err(|error| {
            validation_error(
                "SYNAPSE_CALYX_ACTION_PREDICTOR_ARTIFACT_ID_INVALID",
                format!("decode predictor Blob id {value:?}: {error}"),
                "quarantine the malformed validation row and rerun oracle_validate",
            )
        })?;
    }
    Ok(BlobId::from_bytes(bytes))
}

fn cx_id_population_hash(ids: &[CxId]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-action-validation-excluded-population-v1");
    hasher.update(u64::try_from(ids.len()).unwrap_or(u64::MAX).to_be_bytes());
    for id in ids {
        hasher.update(id.as_bytes());
    }
    hex(&hasher.finalize())
}

fn resource_population_hash(state: &[u8], ids: &[CxId]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-action-resource-cause-population-v1");
    hasher.update(u64::try_from(state.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(state);
    hasher.update(u64::try_from(ids.len()).unwrap_or(u64::MAX).to_be_bytes());
    for id in ids {
        hasher.update(id.as_bytes());
    }
    hex(&hasher.finalize())
}

fn action_registry_serving_slots_hash(slots: &[u16]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-action-causal-registry-serving-slots-v1");
    hasher.update(u64::try_from(slots.len()).unwrap_or(u64::MAX).to_be_bytes());
    for slot in slots {
        hasher.update(slot.to_be_bytes());
    }
    hex(&hasher.finalize())
}

fn guard_profile_key(panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(19);
    key.extend_from_slice(b"profile\0panel\0");
    key.extend_from_slice(&panel_version.to_be_bytes());
    key
}

fn guard_serving_key(panel_version: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(19);
    key.extend_from_slice(b"serving\0panel\0");
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

fn prediction_publication_stale(
    phase: &str,
    source: &str,
    expected: impl std::fmt::Display,
    actual: impl std::fmt::Display,
) -> SynapseCalyxError {
    validation_error(
        "SYNAPSE_CALYX_TYPED_PREDICTION_SOURCE_MOVED_AT_PUBLICATION",
        format!(
            "typed prediction source moved {phase}: source={source} expected={expected} actual={actual}"
        ),
        "read the Answer ledger to determine whether publication crossed its commit boundary; preserve any committed stale Answer as attributable history, then rerun validation/readiness and issue a new prediction against one stable source generation; a stale Answer is never returned as a prediction",
    )
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
