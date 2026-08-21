//! Compact, ledger-bound registry of the deterministic causal views carried by
//! one frozen panel.
//!
//! The registry stores contracts and scalar assay evidence only. It never
//! stores slot vectors, pair matrices, or estimator workspaces, and publishing
//! it never changes the panel lifecycle state.

use std::collections::{BTreeMap, BTreeSet};

use calyx_assay::{ENSEMBLE_CARD_PID_METHOD, EnsembleDecision};
use calyx_aster::cf::ColumnFamily;
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::{
    SynapseCalyxAssayParams, SynapseCalyxEnsembleCardReport, SynapseCalyxError,
    SynapseCalyxExcludedLensCode, SynapseCalyxVault, lens_provenance,
};

pub const SYNAPSE_CAUSAL_VIEW_REGISTRY_SCHEMA_VERSION: u32 = 5;
pub const SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES: usize = 256 * 1024;
pub const SYNAPSE_CAUSAL_VIEW_REGISTRY_MIN_VIEWS_PER_PARENT: usize = 5;
pub const SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_VIEWS_PER_PARENT: usize = 10;
pub const SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES: usize = 269;
/// Writer-sealed complete pre-trigger action-cause record. The Registry's
/// grounded cohort is exactly Reward rows carrying this seal.
pub const SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT: u16 = 125;

const REGISTRY_KEY_PREFIX: &[u8] = b"causal-view-registry\0v5\0";
const REGISTRY_SCOPE_TAG: &[u8] = b"synapse.causal-view-registry.scope.v5";
const REGISTRY_ACTOR: &str = "synapse-causal-view-registry";
const RESOURCE_HEADROOM_SLOT: u16 = 136;

#[derive(Clone, Copy)]
struct PhysicalCausalViewSpec {
    slot: u16,
    lens_name: &'static str,
    parent_atom: &'static str,
    primary_family: SynapseCalyxCausalViewFamily,
    transform: PhysicalTransform,
}

#[derive(Clone, Copy)]
enum PhysicalTransform {
    OneHot { cardinality: u32 },
    UnitRecordVector { dim: u32, source_feature_count: u16 },
}

const ACTION_PHYSICAL_CAUSAL_VIEW_SPECS: [PhysicalCausalViewSpec; 11] = [
    PhysicalCausalViewSpec {
        slot: 126,
        lens_name: "syn.action.command_shape_onehot.v1",
        parent_atom: "syn.action.command_shape",
        primary_family: SynapseCalyxCausalViewFamily::Shape,
        transform: PhysicalTransform::OneHot { cardinality: 6 },
    },
    PhysicalCausalViewSpec {
        slot: 127,
        lens_name: "syn.action.environment_state_onehot.v1",
        parent_atom: "syn.action.environment_state",
        primary_family: SynapseCalyxCausalViewFamily::Level,
        transform: PhysicalTransform::OneHot { cardinality: 4 },
    },
    PhysicalCausalViewSpec {
        slot: 128,
        lens_name: "syn.action.policy_hazard_mask_onehot.v1",
        parent_atom: "syn.action.policy_hazard_mask",
        primary_family: SynapseCalyxCausalViewFamily::Structure,
        transform: PhysicalTransform::OneHot { cardinality: 16 },
    },
    PhysicalCausalViewSpec {
        slot: 129,
        lens_name: "syn.action.timeout_policy_onehot.v1",
        parent_atom: "syn.action.timeout_policy",
        primary_family: SynapseCalyxCausalViewFamily::Clock,
        transform: PhysicalTransform::OneHot { cardinality: 7 },
    },
    PhysicalCausalViewSpec {
        slot: 130,
        lens_name: "syn.action.execution_route_onehot.v1",
        parent_atom: "syn.action.execution_route",
        primary_family: SynapseCalyxCausalViewFamily::Position,
        transform: PhysicalTransform::OneHot { cardinality: 9 },
    },
    PhysicalCausalViewSpec {
        slot: 131,
        lens_name: "syn.action.request_identity_policy_onehot.v1",
        parent_atom: "syn.action.request_identity_policy",
        primary_family: SynapseCalyxCausalViewFamily::Meaning,
        transform: PhysicalTransform::OneHot { cardinality: 9 },
    },
    PhysicalCausalViewSpec {
        slot: 132,
        lens_name: "syn.action.allow_shell_policy_onehot.v1",
        parent_atom: "syn.action.allow_shell_policy",
        primary_family: SynapseCalyxCausalViewFamily::Level,
        transform: PhysicalTransform::OneHot { cardinality: 4 },
    },
    PhysicalCausalViewSpec {
        slot: 133,
        lens_name: "syn.action.executable_resolution_onehot.v1",
        parent_atom: "syn.action.executable_resolution",
        primary_family: SynapseCalyxCausalViewFamily::Structure,
        transform: PhysicalTransform::OneHot { cardinality: 3 },
    },
    PhysicalCausalViewSpec {
        slot: 134,
        lens_name: "syn.action.working_directory_state_onehot.v1",
        parent_atom: "syn.action.working_directory_state",
        primary_family: SynapseCalyxCausalViewFamily::Position,
        transform: PhysicalTransform::OneHot { cardinality: 3 },
    },
    PhysicalCausalViewSpec {
        slot: 135,
        lens_name: "syn.action.host_precondition_state_onehot.v1",
        parent_atom: "syn.action.host_precondition_state",
        primary_family: SynapseCalyxCausalViewFamily::CrossSection,
        transform: PhysicalTransform::OneHot { cardinality: 8 },
    },
    PhysicalCausalViewSpec {
        slot: 136,
        lens_name: "syn.action.resource_headroom.v1",
        parent_atom: "syn.action.resource_headroom",
        primary_family: SynapseCalyxCausalViewFamily::Dispersion,
        transform: PhysicalTransform::UnitRecordVector {
            dim: 16,
            source_feature_count: 11,
        },
    },
];

const CAUSAL_VIEW_FAMILIES: [SynapseCalyxCausalViewFamily; 10] = [
    SynapseCalyxCausalViewFamily::Level,
    SynapseCalyxCausalViewFamily::Change,
    SynapseCalyxCausalViewFamily::Position,
    SynapseCalyxCausalViewFamily::Dispersion,
    SynapseCalyxCausalViewFamily::Shape,
    SynapseCalyxCausalViewFamily::CrossSection,
    SynapseCalyxCausalViewFamily::Clock,
    SynapseCalyxCausalViewFamily::Vintage,
    SynapseCalyxCausalViewFamily::Structure,
    SynapseCalyxCausalViewFamily::Meaning,
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewRegistryScope {
    pub panel_version: u32,
    pub corpus_shard: String,
    pub anchor_kind: String,
}

impl SynapseCalyxCausalViewRegistryScope {
    #[must_use]
    pub fn from_assay(params: &SynapseCalyxAssayParams) -> Self {
        Self {
            panel_version: params.panel_version,
            corpus_shard: params.corpus_shard.clone(),
            anchor_kind: params.anchor_kind.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewFamily {
    Level,
    Change,
    Position,
    Dispersion,
    Shape,
    CrossSection,
    Clock,
    Vintage,
    Structure,
    Meaning,
}

impl SynapseCalyxCausalViewFamily {
    const fn code(self) -> &'static str {
        match self {
            Self::Level => "level",
            Self::Change => "change",
            Self::Position => "position",
            Self::Dispersion => "dispersion",
            Self::Shape => "shape",
            Self::CrossSection => "cross_section",
            Self::Clock => "clock",
            Self::Vintage => "vintage",
            Self::Structure => "structure",
            Self::Meaning => "meaning",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewRuntime {
    CpuDeterministic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewLifecycle {
    ServingCodeFrozen,
    ParkedUnderpowered,
    ParkedCandidate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SynapseCalyxCausalViewTransformSpec {
    FrozenOneHotIndex {
        cardinality: u32,
    },
    FrozenUnitRecordVector {
        dim: u32,
        source_feature_count: u16,
        l2_unit: bool,
        signed_hash: bool,
        collisions_possible: bool,
        collision_evidence_required: bool,
    },
    CandidateLevelBins {
        bins: u16,
    },
    CandidateLagDifference {
        lag: u16,
    },
    CandidateQuantilePosition {
        bins: u16,
    },
    CandidateRollingDispersion {
        window: u16,
        moments: u16,
    },
    CandidateMomentShape {
        moments: u16,
    },
    CandidateCrossSectionBuckets {
        buckets: u16,
    },
    CandidateCyclicClock {
        period_buckets: u16,
    },
    CandidateVintageBuckets {
        buckets: u16,
    },
    CandidateStructuralHash {
        buckets: u16,
    },
    CandidateMeaningHash {
        buckets: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewOutputKind {
    DenseF32,
    DenseSignedHashF32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewOutputContract {
    pub kind: SynapseCalyxCausalViewOutputKind,
    pub dim: u32,
    pub cardinality: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewCpuCostClass {
    Constant,
    BoundedWindow,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewEstimatorCompatibility {
    LogisticProbe,
    KsgMutualInformation,
    LinearCorrelation,
    TransferEntropy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewContract {
    /// `cv:sha256:<digest>` over every field below, excluding `view_id`.
    pub view_id: String,
    pub slot: Option<u16>,
    pub lens_name: Option<String>,
    /// Exact content-addressed Calyx `LensId` for a producing physical view.
    pub lens_id: Option<String>,
    /// SHA-256 of the canonical persisted `LensSpec` bytes.
    pub lens_spec_sha256: Option<String>,
    /// SHA-256 of the frozen ordered vocabulary/formula/source schema that
    /// creates the lens input. Candidates have no physical extractor yet.
    pub extractor_schema_sha256: Option<String>,
    pub parent_atom: String,
    pub family: SynapseCalyxCausalViewFamily,
    pub source_fields: Vec<String>,
    pub transform_id: String,
    pub transform: SynapseCalyxCausalViewTransformSpec,
    pub output: SynapseCalyxCausalViewOutputContract,
    pub pre_trigger_only: bool,
    pub per_row_worst_case_bytes: u32,
    pub cpu_cost_class: SynapseCalyxCausalViewCpuCostClass,
    pub estimator_compatibility: Vec<SynapseCalyxCausalViewEstimatorCompatibility>,
    pub equivalence_class: String,
    /// Exactly one producing contract owns the atom's evidence. Alternative
    /// transforms remain non-owning candidate metadata and cannot inflate the
    /// admitted independent-view count.
    pub equivalence_owner: bool,
    pub producing: bool,
    pub lifecycle: SynapseCalyxCausalViewLifecycle,
    pub refusal: Option<String>,
    pub runtime: SynapseCalyxCausalViewRuntime,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CausalViewContractContent<'a> {
    slot: Option<u16>,
    lens_name: &'a Option<String>,
    lens_id: &'a Option<String>,
    lens_spec_sha256: &'a Option<String>,
    extractor_schema_sha256: &'a Option<String>,
    parent_atom: &'a str,
    family: SynapseCalyxCausalViewFamily,
    source_fields: &'a [String],
    transform_id: &'a str,
    transform: &'a SynapseCalyxCausalViewTransformSpec,
    output: &'a SynapseCalyxCausalViewOutputContract,
    pre_trigger_only: bool,
    per_row_worst_case_bytes: u32,
    cpu_cost_class: SynapseCalyxCausalViewCpuCostClass,
    estimator_compatibility: &'a [SynapseCalyxCausalViewEstimatorCompatibility],
    equivalence_class: &'a str,
    equivalence_owner: bool,
    producing: bool,
    lifecycle: SynapseCalyxCausalViewLifecycle,
    refusal: &'a Option<String>,
    runtime: SynapseCalyxCausalViewRuntime,
}

impl<'a> From<&'a SynapseCalyxCausalViewContract> for CausalViewContractContent<'a> {
    fn from(view: &'a SynapseCalyxCausalViewContract) -> Self {
        Self {
            slot: view.slot,
            lens_name: &view.lens_name,
            lens_id: &view.lens_id,
            lens_spec_sha256: &view.lens_spec_sha256,
            extractor_schema_sha256: &view.extractor_schema_sha256,
            parent_atom: &view.parent_atom,
            family: view.family,
            source_fields: &view.source_fields,
            transform_id: &view.transform_id,
            transform: &view.transform,
            output: &view.output,
            pre_trigger_only: view.pre_trigger_only,
            per_row_worst_case_bytes: view.per_row_worst_case_bytes,
            cpu_cost_class: view.cpu_cost_class,
            estimator_compatibility: &view.estimator_compatibility,
            equivalence_class: &view.equivalence_class,
            equivalence_owner: view.equivalence_owner,
            producing: view.producing,
            lifecycle: view.lifecycle,
            refusal: &view.refusal,
            runtime: view.runtime,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewDecision {
    Keep,
    Park,
    Retire,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewExclusionCode {
    WithheldByCaller,
    ParkedUnderpowered,
    MissingAnchoredCoverage,
    UnusableRepresentation,
    RaggedColumn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewAvailableUnmeasuredCode {
    ObservedCohortConstant,
    DegenerateRedundancySketch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SynapseCalyxCausalViewMeasurement {
    Measured {
        solo_bits: f32,
        solo_ci: [f32; 2],
        panel_without_bits: f32,
        marginal_bits: f32,
        marginal_ci: [f32; 2],
        max_pairwise_corr: f32,
        max_pairwise_nmi: f32,
        diagnostic_assay_decision: SynapseCalyxCausalViewDecision,
        diagnostic_assay_reason: String,
    },
    /// The physical view is complete and valid but did not enter this
    /// estimator. Only an exactly constant observed column has an analytical
    /// zero-information value; a collapsed redundancy sketch remains unknown.
    AvailableUnmeasured {
        code: SynapseCalyxCausalViewAvailableUnmeasuredCode,
        #[serde(skip_serializing_if = "Option::is_none")]
        analytical_incremental_bits: Option<f32>,
        reason: String,
    },
    Excluded {
        code: SynapseCalyxCausalViewExclusionCode,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewResult {
    pub slot: u16,
    pub lens_name: String,
    pub anchored_records_carried: usize,
    pub anchored_coverage: f32,
    pub measurement: SynapseCalyxCausalViewMeasurement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewSelectionPowerState {
    Underpowered,
    Powered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynapseCalyxCausalViewAssociationScope {
    WithinAtomOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewAssociationPolicy {
    pub scope: SynapseCalyxCausalViewAssociationScope,
    pub views_as_stream_nodes: bool,
    pub sibling_pairs_generated: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewResourceAccounting {
    pub registry_value_ceiling_bytes: usize,
    pub contract_count: usize,
    pub producing_contract_count: usize,
    pub serving_contract_count: usize,
    pub parked_underpowered_contract_count: usize,
    pub metadata_only_candidate_contract_count: usize,
    pub cpu_runtime_contract_count: usize,
    pub gpu_runtime_contract_count: usize,
    /// Vector payload stored in the Registry artifact itself. The Registry is
    /// scalar metadata and therefore stores none.
    pub registry_persisted_vector_bytes: u64,
    /// Pair/matrix payload stored in the Registry artifact itself. Sibling
    /// candidates are contracts, never materialized matrices.
    pub registry_persisted_matrix_bytes: u64,
    /// Exact f32 payload produced by the one physical view per atom for one
    /// measured action row. This names the real panel cost instead of hiding it
    /// behind the Registry artifact's zero-vector property.
    pub producing_f32_payload_bytes_per_record: u64,
    /// Worst-case producing f32 payload under this measurement's declared row
    /// ceiling. Serialization/index overhead is intentionally not claimed by
    /// this raw-payload number.
    pub producing_f32_payload_bytes_at_max_records: u64,
    /// Parked sibling candidates are metadata-only and allocate no row payload.
    pub candidate_materialized_payload_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent measured lifecycle and bounded-scan proof predicates must remain explicit in the public evidence envelope"
)]
pub struct SynapseCalyxCausalViewRegistryEvidence {
    pub records_scanned: usize,
    /// True when the bounded scan filled its exact effective record limit.
    /// This conservatively means the scan cannot claim census completeness.
    pub scan_limit_reached: bool,
    pub census_complete: bool,
    pub anchored_records: usize,
    pub selection_power_state: SynapseCalyxCausalViewSelectionPowerState,
    pub observed_sample_count: usize,
    pub required_sample_count: usize,
    pub declared_slots: usize,
    pub declared_slot_ids: Vec<u16>,
    pub physically_available_slots: Vec<u16>,
    pub estimable_slots: Vec<u16>,
    pub anchor_entropy_bits: f32,
    pub panel_bits: f32,
    pub panel_ci: [f32; 2],
    pub effective_rank_features: f32,
    pub sufficient: bool,
    pub deficit_bits: f32,
    pub pairs_monotonicity_floored: usize,
    pub anchor_source_declared: bool,
    pub views: Vec<SynapseCalyxCausalViewResult>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewMeasurementContract {
    pub requested_max_records: usize,
    /// Exact cap actually passed to the ensemble estimator.  This is never a
    /// hidden clamp and is the bound used by resource accounting.
    pub effective_max_records: usize,
    /// Exact row-cohort predicate applied before anchored measurements.
    pub required_record_slots: Vec<u16>,
    pub caller_excluded_slots: Vec<u16>,
    pub registry_parked_underpowered_slots: Vec<u16>,
    pub min_gate_lenses: usize,
    pub estimator: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewRegistry {
    pub schema_version: u32,
    pub generation: u64,
    pub scope: SynapseCalyxCausalViewRegistryScope,
    pub logical_scope_sha256: String,
    /// Sequence visible after the source Assay rows were persisted. It is a
    /// publication frontier, not a claim that the bounded corpus was rescanned
    /// at this exact sequence.
    pub assembled_at_seq: u64,
    /// Exact panel-scoped content watermark before and after the bounded assay.
    /// Publication refuses if it changes during measurement.
    pub source_panel_content_seq: u64,
    /// Exact Anchors CF commit frontier before and after the bounded assay.
    /// This remains separate from the panel watermark because grounded outcomes
    /// can change without changing frozen slot content.
    pub source_anchors_cf_last_commit_seq: u64,
    /// Exact Anchors CF out-of-band mutation epoch paired with
    /// `source_anchors_cf_last_commit_seq`.
    pub source_anchors_cf_out_of_band_epoch: u64,
    pub measurement_contract: SynapseCalyxCausalViewMeasurementContract,
    pub measurement_contract_sha256: String,
    pub catalog: Vec<SynapseCalyxCausalViewContract>,
    pub catalog_sha256: String,
    pub evidence: SynapseCalyxCausalViewRegistryEvidence,
    pub evidence_sha256: String,
    pub association_policy: SynapseCalyxCausalViewAssociationPolicy,
    pub resource_accounting: SynapseCalyxCausalViewResourceAccounting,
    pub ledger_seq: u64,
    pub ledger_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCausalViewRegistry {
    registry: SynapseCalyxCausalViewRegistry,
    registry_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxCausalViewRegistryReadback {
    pub registry: SynapseCalyxCausalViewRegistry,
    pub registry_sha256: String,
    pub physical_key_hex: String,
    pub stored_value_len_bytes: usize,
    pub stored_value_sha256: String,
    pub row_revision_sha256: String,
    pub ledger_entry_kind: String,
    pub ledger_entry_payload_sha256: String,
    pub ledger_entry_self_verifies: bool,
    pub ledger_reference_verified: bool,
    pub existing_identical: bool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CausalViewRegistryLedgerPayload<'a> {
    registry_content_sha256: String,
    schema_version: u32,
    generation: u64,
    logical_scope_sha256: &'a str,
    measurement_contract_sha256: &'a str,
    catalog_sha256: &'a str,
    evidence_sha256: &'a str,
    panel_version: u32,
    source_panel_content_seq: u64,
    source_anchors_cf_last_commit_seq: u64,
    source_anchors_cf_out_of_band_epoch: u64,
    anchor_kind: &'a str,
    view_count: usize,
    anchored_records: usize,
}

impl SynapseCalyxVault {
    /// Reads the exact current panel-scoped content watermark without scanning
    /// or hydrating the corpus.
    /// Reads the latest physical Base-row watermark for one panel.
    ///
    /// # Errors
    ///
    /// Returns a structured error when Base-row decoding fails or sequence
    /// arithmetic cannot be represented.
    pub fn panel_content_seq(&self, panel_version: u32) -> Result<u64, SynapseCalyxError> {
        self.with_panel_read_snapshot(
            panel_version,
            crate::INTELLIGENCE_CORPUS_READER_LEASE_MS,
            |snapshot| Ok(snapshot.derived_content_seq()),
        )
    }

    /// Measures the existing native ensemble card once, projects only compact
    /// scalar evidence for the declared causal views, and atomically publishes
    /// the Registry row with its Assay ledger entry.
    ///
    /// # Errors
    ///
    /// Fails closed when the frozen panel carries fewer than five or more than
    /// ten declared views for a parent atom, any catalog/evidence invariant is
    /// invalid, the row exceeds 256 KiB, or atomic persistence/readback fails.
    #[expect(
        clippy::too_many_lines,
        reason = "measurement, immutable publication, and immediate physical readback are one evidence transaction"
    )]
    pub fn measure_causal_view_registry(
        &self,
        params: &SynapseCalyxAssayParams,
        min_gate_lenses: usize,
    ) -> Result<SynapseCalyxCausalViewRegistryReadback, SynapseCalyxError> {
        let scope = SynapseCalyxCausalViewRegistryScope::from_assay(params);
        validate_scope(&scope)?;
        if !(1..=crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS).contains(&params.max_records) {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RECORD_LIMIT_INVALID",
                format!(
                    "requested max_records={} is outside 1..={}",
                    params.max_records,
                    crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS
                ),
                "supply an explicit max_records inside the inclusive intelligence bound; the Registry never clamps an invalid request",
            ));
        }
        let effective_max_records = params.max_records.min(crate::SYNAPSE_ENSEMBLE_MAX_RECORDS);
        let required_record_slots =
            std::iter::once(SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT).collect::<BTreeSet<_>>();
        if params.required_record_slots != required_record_slots {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_COHORT_INVALID",
                format!(
                    "causal-view Registry requires writer-sealed record slots {required_record_slots:?}, received {:?}",
                    params.required_record_slots
                ),
                "measure the canonical action Registry through its storage MCP surface; the grounded cohort is never inferred from partial slot presence",
            ));
        }
        let source_panel_content_seq = self.panel_content_seq(params.panel_version)?;
        let source_anchors_cf_frontier = self.cf_change_signal(ColumnFamily::Anchors);
        let catalog = causal_view_catalog(params)?;
        let caller_excluded_predictor_slots =
            crate::action_validation::ACTION_CAUSAL_PREDICTOR_SLOTS
                .iter()
                .copied()
                .filter(|slot| params.excluded_slots.contains(slot))
                .collect::<Vec<_>>();
        if !caller_excluded_predictor_slots.is_empty() {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PREDICTOR_VIEW_EXCLUDED",
                format!(
                    "caller excluded immutable action predictor slots {caller_excluded_predictor_slots:?}"
                ),
                "remove every frozen action predictor slot from excluded_slots; the authoritative Registry writer never publishes statistically withheld evidence for a serving cause",
            ));
        }
        let undeclared_excluded_slots = params
            .excluded_slots
            .iter()
            .copied()
            .filter(|slot| !params.lens_names.contains_key(slot))
            .collect::<Vec<_>>();
        if !undeclared_excluded_slots.is_empty() {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EXCLUDED_SLOT_UNDECLARED",
                format!(
                    "caller excluded slots absent from the supplied physical panel catalog: {undeclared_excluded_slots:?}"
                ),
                "supply only declared panel slot ids; an identifier with no physical lane cannot change measurement and is never persisted as causal evidence",
            ));
        }
        let caller_excluded_serving_slots = catalog
            .iter()
            .filter(|view| {
                view.producing
                    && view.lifecycle == SynapseCalyxCausalViewLifecycle::ServingCodeFrozen
            })
            .filter_map(|view| view.slot)
            .filter(|slot| params.excluded_slots.contains(slot))
            .collect::<Vec<_>>();
        if !caller_excluded_serving_slots.is_empty() {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SERVING_VIEW_EXCLUDED",
                format!(
                    "caller excluded immutable serving causal-view slots {caller_excluded_serving_slots:?}"
                ),
                "remove every ServingCodeFrozen slot from excluded_slots; withholding a serving view requires a new immutable catalog/panel generation and cannot be represented as measurement-time exclusion",
            ));
        }
        let registry_parked_underpowered_slots = catalog
            .iter()
            .filter(|view| {
                view.producing
                    && view.lifecycle == SynapseCalyxCausalViewLifecycle::ParkedUnderpowered
            })
            .filter_map(|view| view.slot)
            .collect::<BTreeSet<_>>();
        let mut effective_params = params.clone();
        effective_params.max_records = effective_max_records;
        effective_params
            .excluded_slots
            .extend(registry_parked_underpowered_slots.iter().copied());
        let contract = SynapseCalyxCausalViewMeasurementContract {
            requested_max_records: params.max_records,
            effective_max_records,
            required_record_slots: params.required_record_slots.iter().copied().collect(),
            caller_excluded_slots: params.excluded_slots.iter().copied().collect(),
            registry_parked_underpowered_slots: registry_parked_underpowered_slots
                .iter()
                .copied()
                .collect(),
            min_gate_lenses,
            estimator: ENSEMBLE_CARD_PID_METHOD.to_owned(),
        };
        let association_policy = SynapseCalyxCausalViewAssociationPolicy {
            scope: SynapseCalyxCausalViewAssociationScope::WithinAtomOnly,
            views_as_stream_nodes: false,
            sibling_pairs_generated: 0,
        };
        let resource_accounting = causal_view_resource_accounting(&catalog, effective_max_records)?;
        let logical_scope_sha256 = scope_sha256(&scope)?;
        let measurement_contract_sha256 =
            typed_sha256("causal-view measurement contract", &contract)?;
        let catalog_sha256 = typed_sha256("causal-view catalog", &catalog)?;
        let association_policy_sha256 =
            typed_sha256("causal-view association policy", &association_policy)?;
        let resource_accounting_sha256 =
            typed_sha256("causal-view resource accounting", &resource_accounting)?;

        let existing_before_assay = self.read_causal_view_registry(&scope)?;
        let existing_identical = if let Some(existing) = existing_before_assay.as_ref() {
            let existing_association_policy_sha256 = typed_sha256(
                "stored causal-view association policy",
                &existing.registry.association_policy,
            )?;
            let existing_resource_accounting_sha256 = typed_sha256(
                "stored causal-view resource accounting",
                &existing.registry.resource_accounting,
            )?;
            existing.registry.source_panel_content_seq == source_panel_content_seq
                && existing.registry.source_anchors_cf_last_commit_seq
                    == source_anchors_cf_frontier.0
                && existing.registry.source_anchors_cf_out_of_band_epoch
                    == source_anchors_cf_frontier.1
                && existing.registry.measurement_contract_sha256 == measurement_contract_sha256
                && existing.registry.catalog_sha256 == catalog_sha256
                && existing_association_policy_sha256 == association_policy_sha256
                && existing_resource_accounting_sha256 == resource_accounting_sha256
        } else {
            false
        };
        if existing_identical {
            verify_registry_source_frontiers(
                self,
                params.panel_version,
                source_panel_content_seq,
                source_anchors_cf_frontier,
                "before returning the existing-identical Registry",
            )?;
            let existing = existing_before_assay.ok_or_else(|| {
                registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EXISTING_STATE_LOST",
                    "the existing-identical Registry disappeared from the local verified read",
                    "preserve the Registry CF and inspect the in-process Registry reuse path",
                )
            })?;
            return Ok(SynapseCalyxCausalViewRegistryReadback {
                existing_identical: true,
                ..existing
            });
        }

        let report = self.assay_ensemble_card(&effective_params, min_gate_lenses)?;
        verify_registry_source_frontiers(
            self,
            params.panel_version,
            source_panel_content_seq,
            source_anchors_cf_frontier,
            "during Registry measurement",
        )?;
        if report.card.pid_method != ENSEMBLE_CARD_PID_METHOD {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_ESTIMATOR_CONTRACT_MISMATCH",
                format!(
                    "ensemble card estimator {:?} != frozen Registry estimator {:?}",
                    report.card.pid_method, ENSEMBLE_CARD_PID_METHOD
                ),
                "repair the assay/Registry version skew; an estimator result is never published under a different frozen measurement contract",
            ));
        }
        let evidence = causal_view_evidence(
            &catalog,
            &report,
            effective_max_records,
            &registry_parked_underpowered_slots,
        )?;
        let evidence_sha256 = typed_sha256("causal-view evidence", &evidence)?;

        let publication_base = self.read_causal_view_registry(&scope)?;
        let expected_registry_revision_sha256 = publication_base
            .as_ref()
            .map(|existing| existing.row_revision_sha256.clone());
        let generation = match publication_base {
            Some(existing) => existing.registry.generation.checked_add(1).ok_or_else(|| {
                registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_GENERATION_EXHAUSTED",
                    "causal-view registry generation reached u64::MAX",
                    "preserve the Registry and Ledger rows and repair the impossible generation before republishing",
                )
            })?,
            None => 1,
        };
        let draft = SynapseCalyxCausalViewRegistry {
            schema_version: SYNAPSE_CAUSAL_VIEW_REGISTRY_SCHEMA_VERSION,
            generation,
            scope: scope.clone(),
            logical_scope_sha256,
            assembled_at_seq: self.latest_seq(),
            source_panel_content_seq,
            source_anchors_cf_last_commit_seq: source_anchors_cf_frontier.0,
            source_anchors_cf_out_of_band_epoch: source_anchors_cf_frontier.1,
            measurement_contract: contract,
            measurement_contract_sha256,
            catalog,
            catalog_sha256,
            evidence,
            evidence_sha256,
            association_policy,
            resource_accounting,
            ledger_seq: 0,
            ledger_hash: String::new(),
        };
        validate_registry(&draft, &scope)?;
        preflight_stored_size(&draft)?;

        let payload = ledger_payload_bytes(&draft)?;
        let key = registry_key(&scope)?;
        let subject = scope_wire_bytes(&scope)?;
        let mut committed: Option<SynapseCalyxCausalViewRegistry> = None;
        let mut publication_guard_error: Option<SynapseCalyxError> = None;
        let commit_result = self
            .vault
            .append_ledger_entry_with_rows(
                EntryKind::Assay,
                SubjectId::Query(subject),
                payload,
                ActorId::Service(REGISTRY_ACTOR.to_owned()),
                |ledger_ref| {
                    // `append_ledger_entry_with_rows` invokes this closure while
                    // holding Aster's process + cross-process durable commit
                    // boundary. Recheck both source frontiers and the exact
                    // Registry revision here, after the ledger member has been
                    // staged but before either row can become visible. This is
                    // the publication precondition; the earlier check only
                    // proves the bounded assay itself observed one frontier.
                    if let Err(error) = verify_registry_source_frontiers(
                        self,
                        params.panel_version,
                        source_panel_content_seq,
                        source_anchors_cf_frontier,
                        "at the atomic Registry+Ledger publication boundary",
                    ) {
                        let message = error.to_string();
                        publication_guard_error = Some(error);
                        return Err(calyx_core::CalyxError::ledger_group_commit_failed(
                            message,
                        ));
                    }
                    let actual_registry_revision_sha256 = match self
                        .read_cf_latest_revisioned(ColumnFamily::Registry, &key)
                    {
                        Ok(row) => row.map(|row| crate::hex_bytes(&row.revision_sha256)),
                        Err(error) => {
                            let message = error.to_string();
                            publication_guard_error = Some(error);
                            return Err(calyx_core::CalyxError::ledger_group_commit_failed(
                                message,
                            ));
                        }
                    };
                    if actual_registry_revision_sha256 != expected_registry_revision_sha256 {
                        let error = registry_error(
                            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PUBLICATION_CONFLICT",
                            format!(
                                "causal-view Registry revision moved before publication: expected={expected_registry_revision_sha256:?} actual={actual_registry_revision_sha256:?}"
                            ),
                            "read the current Registry row and remeasure from its equal-or-newer source frontier; a concurrent generation is never overwritten",
                        );
                        let message = error.to_string();
                        publication_guard_error = Some(error);
                        return Err(calyx_core::CalyxError::ledger_group_commit_failed(
                            message,
                        ));
                    }
                    let mut registry = draft.clone();
                    registry.ledger_seq = ledger_ref.seq;
                    registry.ledger_hash = crate::hex_bytes(&ledger_ref.hash);
                    let registry_bytes = serde_json::to_vec(&registry).map_err(|error| {
                        calyx_core::CalyxError::ledger_group_commit_failed(format!(
                            "encode causal-view registry: {error}"
                        ))
                    })?;
                    let stored = StoredCausalViewRegistry {
                        registry: registry.clone(),
                        registry_sha256: crate::sha256_hex(&registry_bytes),
                    };
                    let value = serde_json::to_vec(&stored).map_err(|error| {
                        calyx_core::CalyxError::ledger_group_commit_failed(format!(
                            "encode stored causal-view registry: {error}"
                        ))
                    })?;
                    if value.len() > SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES {
                        return Err(calyx_core::CalyxError::ledger_group_commit_failed(format!(
                            "causal-view Registry value is {} bytes; ceiling is {} bytes",
                            value.len(),
                            SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES
                        )));
                    }
                    committed = Some(registry);
                    Ok(vec![(ColumnFamily::Registry, key.clone(), value)])
                },
            );
        if let Some(error) = publication_guard_error {
            if commit_result.is_ok() {
                return Err(registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PUBLICATION_GUARD_BYPASSED",
                    "Registry publication reported success after its in-lock source/revision guard refused",
                    "preserve the WAL and Registry/Ledger rows and repair the atomic publication boundary before trusting any causal evidence",
                ));
            }
            return Err(error);
        }
        commit_result.map_err(|error| {
            SynapseCalyxError::from_calyx(
                "atomically persist causal-view Registry and Assay ledger entry",
                &error,
            )
        })?;
        let expected = committed.ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_COMMIT_MISSING",
                "causal-view Registry commit completed without materializing the typed row",
                "preserve the WAL and inspect the ledger group-commit closure",
            )
        })?;
        let actual = self.read_causal_view_registry(&scope)?.ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_READBACK_MISSING",
                "causal-view Registry row is absent immediately after atomic commit",
                "preserve the WAL and inspect the Registry CF publication boundary",
            )
        })?;
        if actual.registry != expected {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_READBACK_MISMATCH",
                "causal-view Registry physical readback differs from the atomically committed value",
                "preserve the Registry and Ledger rows and inspect concurrent writers and MVCC publication",
            ));
        }
        verify_registry_source_frontiers(
            self,
            params.panel_version,
            source_panel_content_seq,
            source_anchors_cf_frontier,
            "after the atomic Registry+Ledger publication readback",
        )?;
        Ok(actual)
    }

    /// Reads and integrity-verifies the exact latest Registry CF row for one
    /// logical causal-view scope.
    ///
    /// # Errors
    ///
    /// Fails closed if the physical row, any typed content digest, or its
    /// referenced Assay ledger entry is missing, corrupt, or inconsistent.
    pub fn read_causal_view_registry(
        &self,
        scope: &SynapseCalyxCausalViewRegistryScope,
    ) -> Result<Option<SynapseCalyxCausalViewRegistryReadback>, SynapseCalyxError> {
        validate_scope(scope)?;
        let key = registry_key(scope)?;
        let Some(row) = self.read_cf_latest_revisioned(ColumnFamily::Registry, &key)? else {
            return Ok(None);
        };
        if row.value.len() > SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
                format!(
                    "stored causal-view Registry value is {} bytes; ceiling is {} bytes",
                    row.value.len(),
                    SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES
                ),
                "quarantine the oversized Registry row and republish from the bounded typed catalog",
            ));
        }
        let stored: StoredCausalViewRegistry =
            serde_json::from_slice(&row.value).map_err(|error| {
                registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CORRUPT",
                    format!("decode causal-view Registry row: {error}"),
                    "quarantine the corrupt Registry row and republish it from frozen panel metadata",
                )
            })?;
        validate_registry(&stored.registry, scope)?;
        verify_typed_hash(
            "measurement contract",
            &stored.registry.measurement_contract,
            &stored.registry.measurement_contract_sha256,
        )?;
        verify_typed_hash(
            "catalog",
            &stored.registry.catalog,
            &stored.registry.catalog_sha256,
        )?;
        verify_typed_hash(
            "evidence",
            &stored.registry.evidence,
            &stored.registry.evidence_sha256,
        )?;
        let registry_bytes = serde_json::to_vec(&stored.registry)
            .map_err(|error| encode_error("causal-view Registry readback", &error))?;
        let registry_sha256 = crate::sha256_hex(&registry_bytes);
        if registry_sha256 != stored.registry_sha256 {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_INTEGRITY_FAILED",
                format!(
                    "causal-view Registry content hash {registry_sha256} != stored {}",
                    stored.registry_sha256
                ),
                "quarantine the tampered Registry row and republish it from the measured Assay evidence",
            ));
        }
        let ledger_entry = self.read_ledger_entry(stored.registry.ledger_seq)?;
        let expected_payload_sha256 = crate::sha256_hex(&ledger_payload_bytes(&stored.registry)?);
        let ledger_entry_kind = ledger_entry.kind.clone().unwrap_or_default();
        let ledger_entry_payload_sha256 = ledger_entry.payload_sha256.clone().unwrap_or_default();
        let ledger_entry_self_verifies = ledger_entry.self_verifies.unwrap_or(false);
        let ledger_reference_verified = ledger_entry.present
            && ledger_entry_kind == EntryKind::Assay.as_str()
            && ledger_entry.entry_hash.as_deref() == Some(stored.registry.ledger_hash.as_str())
            && ledger_entry_payload_sha256 == expected_payload_sha256
            && ledger_entry_self_verifies;
        if !ledger_reference_verified {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_LEDGER_UNBOUND",
                format!(
                    "causal-view Registry references ledger seq={} hash={}, but physical readback has present={} kind={:?} hash={:?} payload_sha256={:?} expected_payload_sha256={} self_verifies={:?}",
                    stored.registry.ledger_seq,
                    stored.registry.ledger_hash,
                    ledger_entry.present,
                    ledger_entry.kind,
                    ledger_entry.entry_hash,
                    ledger_entry.payload_sha256,
                    expected_payload_sha256,
                    ledger_entry.self_verifies,
                ),
                "quarantine the unbound Registry row and republish it through the atomic Registry+Assay-ledger writer",
            ));
        }
        Ok(Some(SynapseCalyxCausalViewRegistryReadback {
            registry: stored.registry,
            registry_sha256,
            physical_key_hex: crate::hex_bytes(&key),
            stored_value_len_bytes: row.value.len(),
            stored_value_sha256: crate::sha256_hex(&row.value),
            row_revision_sha256: crate::hex_bytes(&row.revision_sha256),
            ledger_entry_kind,
            ledger_entry_payload_sha256,
            ledger_entry_self_verifies,
            ledger_reference_verified,
            existing_identical: false,
        }))
    }
}

fn verify_registry_source_frontiers(
    vault: &SynapseCalyxVault,
    panel_version: u32,
    expected_panel_content_seq: u64,
    expected_anchors_cf_frontier: (u64, u64),
    phase: &str,
) -> Result<(), SynapseCalyxError> {
    let actual_panel_content_seq = vault.panel_content_seq(panel_version)?;
    let actual_anchors_cf_frontier = vault.cf_change_signal(ColumnFamily::Anchors);
    if actual_panel_content_seq != expected_panel_content_seq
        || actual_anchors_cf_frontier != expected_anchors_cf_frontier
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SOURCE_MOVED",
            format!(
                "Registry source moved {phase}: panel={panel_version} panel_content_seq={expected_panel_content_seq}->{actual_panel_content_seq} anchors_cf_frontier={expected_anchors_cf_frontier:?}->{actual_anchors_cf_frontier:?}"
            ),
            "remeasure against one quiescent panel and Anchors CF frontier; mixed-frontier causal evidence is never reused or published",
        ));
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "the bounded physical roster and its ten immutable candidate families are validated together"
)]
fn causal_view_catalog(
    params: &SynapseCalyxAssayParams,
) -> Result<Vec<SynapseCalyxCausalViewContract>, SynapseCalyxError> {
    let panel_version = params.panel_version;
    let panel_slots = lens_provenance::syn_panel_slots(panel_version);
    if panel_slots.is_empty() {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PANEL_UNKNOWN",
            format!("panel {panel_version} has no frozen slot metadata"),
            "publish the panel slot/source-field metadata before measuring its causal-view registry",
        ));
    }
    let mut physical = Vec::new();
    for slot in panel_slots {
        let lens_name = lens_provenance::syn_slot_declared_lens(slot).ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("panel {panel_version} slot {slot} has no declared frozen lens name"),
                "repair the frozen lens provenance table; registry publication never guesses a lens identity",
            )
        })?;
        let Some(spec) = ACTION_PHYSICAL_CAUSAL_VIEW_SPECS
            .iter()
            .find(|spec| spec.lens_name == lens_name)
        else {
            continue;
        };
        if slot != spec.slot {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_ROSTER_INVALID",
                format!(
                    "causal lens {lens_name} is physically bound to slot {slot}, expected immutable slot {}",
                    spec.slot
                ),
                "publish a new panel generation for any slot move; never reinterpret the frozen action causal-view roster",
            ));
        }
        let source_fields = lens_provenance::syn_slot_source_fields(slot).ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("causal-view slot {slot} ({lens_name}) has no declared source fields"),
                "declare the exact independently measured source fields before publishing this view",
            )
        })?;
        if source_fields.is_empty() {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("causal-view slot {slot} ({lens_name}) declares an empty source-field set"),
                "declare at least one exact source field; a causal view with no measured atom is undefined",
            ));
        }
        physical.push((*spec, slot, source_fields));
    }
    if physical.len() != ACTION_PHYSICAL_CAUSAL_VIEW_SPECS.len() {
        let present = physical
            .iter()
            .map(|(spec, slot, _)| format!("{}@{slot}", spec.lens_name))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_UNDERPOPULATED",
            format!(
                "panel {panel_version} carries {} of {} frozen primary causal atoms [{present}]",
                physical.len(),
                ACTION_PHYSICAL_CAUSAL_VIEW_SPECS.len()
            ),
            "publish and backfill the complete immutable compact action panel before measuring its causal-view registry",
        ));
    }
    physical.sort_by_key(|(_, slot, _)| *slot);
    let mut catalog = Vec::with_capacity(physical.len().saturating_mul(CAUSAL_VIEW_FAMILIES.len()));
    for (spec, slot, source_fields) in physical {
        let physical_binding = params.physical_lens_bindings.get(&slot).ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_BINDING_MISSING",
                format!(
                    "producing causal-view slot {slot} ({}) has no physical LensId/LensSpec/extractor binding",
                    spec.lens_name
                ),
                "measure through the panel-owning storage surface so every producing view is bound to the exact frozen lens and extractor contract",
            )
        })?;
        for family in CAUSAL_VIEW_FAMILIES {
            let producing = family == spec.primary_family;
            let lifecycle = if !producing {
                SynapseCalyxCausalViewLifecycle::ParkedCandidate
            } else if slot == RESOURCE_HEADROOM_SLOT {
                SynapseCalyxCausalViewLifecycle::ParkedUnderpowered
            } else {
                SynapseCalyxCausalViewLifecycle::ServingCodeFrozen
            };
            let refusal = match lifecycle {
                SynapseCalyxCausalViewLifecycle::ServingCodeFrozen => None,
                SynapseCalyxCausalViewLifecycle::ParkedUnderpowered => Some(format!(
                    "parked_underpowered: physical slot {slot} is collected for coverage but excluded from serving, readiness, and panel bits; require at least {SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES} complete anchored observations and a future immutable promotion"
                )),
                SynapseCalyxCausalViewLifecycle::ParkedCandidate => Some(
                    "metadata_only_candidate: no frozen producing lens exists for this atom/family; commission, power, and publish a new immutable slot before admission"
                        .to_owned(),
                ),
            };
            let (transform_id, transform, output, cpu_cost_class) = if producing {
                physical_transform_contract(spec.transform)
            } else {
                candidate_transform_contract(family)
            };
            let per_row_worst_case_bytes = output.dim.checked_mul(4).ok_or_else(|| {
                registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
                    format!(
                        "causal view {} family {} output dim {} overflows its f32 row-byte contract",
                        spec.parent_atom,
                        family.code(),
                        output.dim
                    ),
                    "bound the immutable candidate output so dim*4 fits u32",
                )
            })?;
            let mut contract = SynapseCalyxCausalViewContract {
                view_id: String::new(),
                slot: producing.then_some(slot),
                lens_name: producing.then(|| spec.lens_name.to_owned()),
                lens_id: producing.then(|| physical_binding.lens_id.clone()),
                lens_spec_sha256: producing.then(|| physical_binding.lens_spec_sha256.clone()),
                extractor_schema_sha256: producing
                    .then(|| physical_binding.extractor_schema_sha256.clone()),
                parent_atom: spec.parent_atom.to_owned(),
                family,
                source_fields: source_fields
                    .iter()
                    .map(|field| (*field).to_owned())
                    .collect(),
                transform_id,
                transform,
                output,
                pre_trigger_only: true,
                per_row_worst_case_bytes,
                cpu_cost_class,
                estimator_compatibility: estimator_compatibility(family),
                equivalence_class: format!("{}.grounded_evidence", spec.parent_atom),
                equivalence_owner: producing,
                producing,
                lifecycle,
                refusal,
                runtime: SynapseCalyxCausalViewRuntime::CpuDeterministic,
            };
            contract.view_id = content_addressed_view_id(&contract)?;
            catalog.push(contract);
        }
    }
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn physical_transform_contract(
    transform: PhysicalTransform,
) -> (
    String,
    SynapseCalyxCausalViewTransformSpec,
    SynapseCalyxCausalViewOutputContract,
    SynapseCalyxCausalViewCpuCostClass,
) {
    match transform {
        PhysicalTransform::OneHot { cardinality } => (
            "syn.transform.frozen_one_hot_index.v1".to_owned(),
            SynapseCalyxCausalViewTransformSpec::FrozenOneHotIndex { cardinality },
            SynapseCalyxCausalViewOutputContract {
                kind: SynapseCalyxCausalViewOutputKind::DenseF32,
                dim: cardinality,
                cardinality: Some(cardinality),
            },
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        PhysicalTransform::UnitRecordVector {
            dim,
            source_feature_count,
        } => (
            "syn.transform.frozen_unit_record_vector.v1".to_owned(),
            SynapseCalyxCausalViewTransformSpec::FrozenUnitRecordVector {
                dim,
                source_feature_count,
                l2_unit: true,
                signed_hash: true,
                collisions_possible: true,
                collision_evidence_required: true,
            },
            SynapseCalyxCausalViewOutputContract {
                kind: SynapseCalyxCausalViewOutputKind::DenseSignedHashF32,
                dim,
                cardinality: None,
            },
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
    }
}

fn candidate_transform_contract(
    family: SynapseCalyxCausalViewFamily,
) -> (
    String,
    SynapseCalyxCausalViewTransformSpec,
    SynapseCalyxCausalViewOutputContract,
    SynapseCalyxCausalViewCpuCostClass,
) {
    let (transform_id, transform, output_kind, dim, cardinality, cpu_cost_class) = match family {
        SynapseCalyxCausalViewFamily::Level => (
            "syn.transform.candidate.level_bins8.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateLevelBins { bins: 8 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            8,
            Some(8),
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::Change => (
            "syn.transform.candidate.lag_difference1.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateLagDifference { lag: 1 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            1,
            None,
            SynapseCalyxCausalViewCpuCostClass::BoundedWindow,
        ),
        SynapseCalyxCausalViewFamily::Position => (
            "syn.transform.candidate.quantile_position8.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateQuantilePosition { bins: 8 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            8,
            Some(8),
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::Dispersion => (
            "syn.transform.candidate.rolling_dispersion16x4.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateRollingDispersion {
                window: 16,
                moments: 4,
            },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            4,
            None,
            SynapseCalyxCausalViewCpuCostClass::BoundedWindow,
        ),
        SynapseCalyxCausalViewFamily::Shape => (
            "syn.transform.candidate.moment_shape4.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateMomentShape { moments: 4 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            4,
            None,
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::CrossSection => (
            "syn.transform.candidate.cross_section_buckets8.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateCrossSectionBuckets { buckets: 8 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            8,
            Some(8),
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::Clock => (
            "syn.transform.candidate.cyclic_clock8.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateCyclicClock { period_buckets: 8 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            2,
            None,
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::Vintage => (
            "syn.transform.candidate.vintage_buckets8.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateVintageBuckets { buckets: 8 },
            SynapseCalyxCausalViewOutputKind::DenseF32,
            8,
            Some(8),
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::Structure => (
            "syn.transform.candidate.structural_hash16.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateStructuralHash { buckets: 16 },
            SynapseCalyxCausalViewOutputKind::DenseSignedHashF32,
            16,
            None,
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
        SynapseCalyxCausalViewFamily::Meaning => (
            "syn.transform.candidate.meaning_hash16.v1",
            SynapseCalyxCausalViewTransformSpec::CandidateMeaningHash { buckets: 16 },
            SynapseCalyxCausalViewOutputKind::DenseSignedHashF32,
            16,
            None,
            SynapseCalyxCausalViewCpuCostClass::Constant,
        ),
    };
    (
        transform_id.to_owned(),
        transform,
        SynapseCalyxCausalViewOutputContract {
            kind: output_kind,
            dim,
            cardinality,
        },
        cpu_cost_class,
    )
}

fn estimator_compatibility(
    family: SynapseCalyxCausalViewFamily,
) -> Vec<SynapseCalyxCausalViewEstimatorCompatibility> {
    let mut compatibility = vec![
        SynapseCalyxCausalViewEstimatorCompatibility::LogisticProbe,
        SynapseCalyxCausalViewEstimatorCompatibility::KsgMutualInformation,
        SynapseCalyxCausalViewEstimatorCompatibility::LinearCorrelation,
    ];
    if matches!(
        family,
        SynapseCalyxCausalViewFamily::Change
            | SynapseCalyxCausalViewFamily::Clock
            | SynapseCalyxCausalViewFamily::Vintage
    ) {
        compatibility.push(SynapseCalyxCausalViewEstimatorCompatibility::TransferEntropy);
    }
    compatibility
}

fn content_addressed_view_id(
    contract: &SynapseCalyxCausalViewContract,
) -> Result<String, SynapseCalyxError> {
    let digest = typed_sha256(
        "causal-view contract content",
        &CausalViewContractContent::from(contract),
    )?;
    Ok(format!("cv:sha256:{digest}"))
}

#[expect(
    clippy::too_many_lines,
    reason = "one ensemble scan is projected into complete per-producing-slot evidence without a second corpus pass"
)]
fn causal_view_evidence(
    catalog: &[SynapseCalyxCausalViewContract],
    report: &SynapseCalyxEnsembleCardReport,
    effective_max_records: usize,
    registry_parked_underpowered_slots: &BTreeSet<u16>,
) -> Result<SynapseCalyxCausalViewRegistryEvidence, SynapseCalyxError> {
    let measured = report
        .card
        .lenses
        .iter()
        .map(|lens| (lens.slot.get(), lens))
        .collect::<BTreeMap<_, _>>();
    let excluded = report
        .excluded_lenses
        .iter()
        .map(|lens| (lens.slot, lens))
        .collect::<BTreeMap<_, _>>();
    let producing = catalog.iter().filter(|view| view.producing);
    let mut views = Vec::with_capacity(producing.clone().count());
    for contract in producing {
        let slot = contract.slot.ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("producing view {} has no physical slot", contract.view_id),
                "repair the immutable producing contract; metadata candidates must never masquerade as physical evidence",
            )
        })?;
        let lens_name = contract.lens_name.as_deref().ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!(
                    "producing view {} has no frozen lens name",
                    contract.view_id
                ),
                "repair the immutable producing contract with its exact registered lens identity",
            )
        })?;
        let anchored_records_carried = report
            .slot_anchored_coverage
            .get(&slot)
            .copied()
            .ok_or_else(|| {
                registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EVIDENCE_INCOMPLETE",
                    format!("producing slot {slot} ({lens_name}) has no anchored coverage count"),
                    "repair the single-pass ensemble accounting so every declared physical slot reports coverage",
                )
            })?;
        let carried = u16::try_from(anchored_records_carried).map_err(|_| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
                format!(
                    "slot {slot} ({lens_name}) anchored coverage count {anchored_records_carried} exceeds the frozen 20,000-record assay bound"
                ),
                "keep registry measurement within max_records <= 20000",
            )
        })?;
        let anchored_total = u16::try_from(report.anchored_records).map_err(|_| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
                format!(
                    "anchored record count {} exceeds the frozen 20,000-record assay bound",
                    report.anchored_records
                ),
                "keep registry measurement within max_records <= 20000",
            )
        })?;
        if anchored_total == 0 {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EVIDENCE_INCOMPLETE",
                "ensemble report contains zero anchored records",
                "attach grounded anchors and satisfy the assay sample floor before measuring the registry",
            ));
        }
        let anchored_coverage = f32::from(carried) / f32::from(anchored_total);
        ensure_finite_values(
            &format!("slot {slot} ({lens_name}) coverage"),
            &[anchored_coverage],
        )?;
        let measurement = if registry_parked_underpowered_slots.contains(&slot) {
            if !excluded.contains_key(&slot) {
                return Err(registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EVIDENCE_INCOMPLETE",
                    format!(
                        "parked-underpowered slot {slot} ({lens_name}) entered the ensemble card"
                    ),
                    "exclude every parked-underpowered physical slot before the assay; it may report coverage but cannot contribute serving bits",
                ));
            }
            SynapseCalyxCausalViewMeasurement::Excluded {
                code: SynapseCalyxCausalViewExclusionCode::ParkedUnderpowered,
                reason: format!(
                    "parked_underpowered: carried by {anchored_records_carried} of {} anchored records; required_sample_count={SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES}; serving/readiness/panel-bit contribution is refused until a future immutable promotion",
                    report.anchored_records
                ),
            }
        } else if let Some(lens) = measured.get(&slot) {
            ensure_finite_values(
                &format!("slot {slot} ({lens_name})"),
                &[
                    lens.solo_bits,
                    lens.solo_ci[0],
                    lens.solo_ci[1],
                    lens.panel_without_bits,
                    lens.marginal_bits,
                    lens.marginal_ci[0],
                    lens.marginal_ci[1],
                    lens.max_pairwise_corr,
                    lens.max_pairwise_nmi,
                ],
            )?;
            SynapseCalyxCausalViewMeasurement::Measured {
                solo_bits: lens.solo_bits,
                solo_ci: lens.solo_ci,
                panel_without_bits: lens.panel_without_bits,
                marginal_bits: lens.marginal_bits,
                marginal_ci: lens.marginal_ci,
                max_pairwise_corr: lens.max_pairwise_corr,
                max_pairwise_nmi: lens.max_pairwise_nmi,
                diagnostic_assay_decision: match lens.decision {
                    EnsembleDecision::Keep => SynapseCalyxCausalViewDecision::Keep,
                    EnsembleDecision::Park => SynapseCalyxCausalViewDecision::Park,
                    EnsembleDecision::Retire => SynapseCalyxCausalViewDecision::Retire,
                },
                diagnostic_assay_reason: lens.decision_reason.clone(),
            }
        } else if let Some(lens) = excluded.get(&slot) {
            match lens.code {
                SynapseCalyxExcludedLensCode::ObservedCohortConstant => {
                    SynapseCalyxCausalViewMeasurement::AvailableUnmeasured {
                        code: SynapseCalyxCausalViewAvailableUnmeasuredCode::ObservedCohortConstant,
                        analytical_incremental_bits: Some(0.0),
                        reason: lens.reason.clone(),
                    }
                }
                SynapseCalyxExcludedLensCode::DegenerateRedundancySketch => {
                    SynapseCalyxCausalViewMeasurement::AvailableUnmeasured {
                        code: SynapseCalyxCausalViewAvailableUnmeasuredCode::DegenerateRedundancySketch,
                        analytical_incremental_bits: None,
                        reason: lens.reason.clone(),
                    }
                }
                code => SynapseCalyxCausalViewMeasurement::Excluded {
                    code: match code {
                        SynapseCalyxExcludedLensCode::WithheldByCaller => {
                            SynapseCalyxCausalViewExclusionCode::WithheldByCaller
                        }
                        SynapseCalyxExcludedLensCode::MissingAnchoredCoverage => {
                            SynapseCalyxCausalViewExclusionCode::MissingAnchoredCoverage
                        }
                        SynapseCalyxExcludedLensCode::UnusableRepresentation => {
                            SynapseCalyxCausalViewExclusionCode::UnusableRepresentation
                        }
                        SynapseCalyxExcludedLensCode::RaggedColumn => {
                            SynapseCalyxCausalViewExclusionCode::RaggedColumn
                        }
                        SynapseCalyxExcludedLensCode::ObservedCohortConstant
                        | SynapseCalyxExcludedLensCode::DegenerateRedundancySketch => {
                            unreachable!("available-unmeasured codes handled above")
                        }
                    },
                    reason: lens.reason.clone(),
                },
            }
        } else {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EVIDENCE_INCOMPLETE",
                format!(
                    "causal-view slot {slot} ({lens_name}) is neither measured nor named in the ensemble exclusions"
                ),
                "repair the ensemble capability-card accounting; every declared view must produce evidence or a named exclusion",
            ));
        };
        views.push(SynapseCalyxCausalViewResult {
            slot,
            lens_name: lens_name.to_owned(),
            anchored_records_carried,
            anchored_coverage,
            measurement,
        });
    }
    ensure_finite_values(
        "ensemble panel",
        &[
            report.card.anchor_entropy_bits,
            report.card.panel_bits,
            report.card.panel_ci[0],
            report.card.panel_ci[1],
            report.card.n_eff,
            report.card.deficit_bits,
        ],
    )?;
    let observed_sample_count = views
        .iter()
        .map(|view| view.anchored_records_carried)
        .min()
        .unwrap_or_default();
    Ok(SynapseCalyxCausalViewRegistryEvidence {
        records_scanned: report.records_scanned,
        scan_limit_reached: report.records_scanned == effective_max_records,
        census_complete: report.records_scanned < effective_max_records,
        anchored_records: report.anchored_records,
        selection_power_state: if observed_sample_count
            < SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES
        {
            SynapseCalyxCausalViewSelectionPowerState::Underpowered
        } else {
            SynapseCalyxCausalViewSelectionPowerState::Powered
        },
        observed_sample_count,
        required_sample_count: SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES,
        declared_slots: report.declared_slots,
        declared_slot_ids: report.declared_slot_ids.clone(),
        physically_available_slots: report.physically_available_slots.clone(),
        estimable_slots: report.estimable_slots.clone(),
        anchor_entropy_bits: report.card.anchor_entropy_bits,
        panel_bits: report.card.panel_bits,
        panel_ci: report.card.panel_ci,
        effective_rank_features: report.card.n_eff,
        sufficient: report.card.sufficient,
        deficit_bits: report.card.deficit_bits,
        pairs_monotonicity_floored: report.card.pairs_monotonicity_floored,
        anchor_source_declared: report.anchor_source_declared,
        views,
    })
}

fn validate_scope(scope: &SynapseCalyxCausalViewRegistryScope) -> Result<(), SynapseCalyxError> {
    if scope.panel_version == 0 {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SCOPE_INVALID",
            "causal-view Registry scope has panel_version=0",
            "supply the exact positive frozen panel version",
        ));
    }
    validate_identity("corpus_shard", &scope.corpus_shard)?;
    validate_identity("anchor_kind", &scope.anchor_kind)
}

fn validate_identity(label: &str, value: &str) -> Result<(), SynapseCalyxError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.as_bytes().contains(&0) || trimmed.len() > 512 {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SCOPE_INVALID",
            format!(
                "causal-view Registry {label} must be 1..=512 non-NUL UTF-8 bytes, got {} bytes",
                value.len()
            ),
            "supply the exact bounded non-empty scope identity",
        ));
    }
    Ok(())
}

fn validate_hex_digest(
    label: &str,
    value: &str,
    expected_len: usize,
) -> Result<(), SynapseCalyxError> {
    if value.len() != expected_len || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_BINDING_INVALID",
            format!("{label} must be exactly {expected_len} hexadecimal bytes, got {value:?}"),
            "reconstruct the exact frozen panel contract and republish its canonical content digest",
        ));
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "the catalog gate checks identity, transform, cost, lifecycle, and parent-equivalence invariants together"
)]
fn validate_catalog(catalog: &[SynapseCalyxCausalViewContract]) -> Result<(), SynapseCalyxError> {
    let expected_contracts = ACTION_PHYSICAL_CAUSAL_VIEW_SPECS
        .len()
        .checked_mul(CAUSAL_VIEW_FAMILIES.len())
        .ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                "causal-view roster cardinality overflowed usize",
                "preserve the build and inspect the immutable causal-view roster",
            )
        })?;
    if catalog.len() != expected_contracts {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_UNDERPOPULATED",
            format!(
                "causal-view catalog has {} contracts; exact immutable roster requires {expected_contracts}",
                catalog.len()
            ),
            "republish all eleven atomic parents with exactly the ten frozen F1-F10 family contracts",
        ));
    }
    let mut slots = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut view_ids = BTreeSet::new();
    let mut parent_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut parent_families: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut parent_producers: BTreeMap<&str, usize> = BTreeMap::new();
    let mut parent_serving: BTreeMap<&str, usize> = BTreeMap::new();
    let mut parent_owners: BTreeMap<&str, usize> = BTreeMap::new();
    for view in catalog {
        if !view_ids.insert(view.view_id.as_str()) {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("duplicate causal-view id {}", view.view_id),
                "give every physical or candidate causal view one stable identity",
            ));
        }
        validate_identity("view_id", &view.view_id)?;
        validate_identity("parent_atom", &view.parent_atom)?;
        validate_identity("transform_id", &view.transform_id)?;
        validate_identity("equivalence_class", &view.equivalence_class)?;
        let expected_view_id = content_addressed_view_id(view)?;
        if view.view_id != expected_view_id {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CONTENT_ID_INVALID",
                format!(
                    "causal view id {} does not match full-contract content id {expected_view_id}",
                    view.view_id
                ),
                "recompute view_id as cv:sha256:<canonical typed contract digest>, excluding only the id itself",
            ));
        }
        if let Some(slot) = view.slot
            && !slots.insert(slot)
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("duplicate physical causal-view slot {slot}"),
                "bind each producing contract to exactly one unique frozen slot",
            ));
        }
        if let Some(name) = &view.lens_name {
            validate_identity("lens_name", name)?;
            if !names.insert(name.as_str()) {
                return Err(registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                    format!("duplicate physical causal-view lens name {name}"),
                    "bind each producing contract to exactly one unique frozen lens identity",
                ));
            }
        }
        match (
            view.producing,
            &view.lens_id,
            &view.lens_spec_sha256,
            &view.extractor_schema_sha256,
        ) {
            (true, Some(lens_id), Some(spec_sha), Some(extractor_sha)) => {
                validate_hex_digest("lens_id", lens_id, 32)?;
                validate_hex_digest("lens_spec_sha256", spec_sha, 64)?;
                validate_hex_digest("extractor_schema_sha256", extractor_sha, 64)?;
            }
            (false, None, None, None) => {}
            _ => {
                return Err(registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_BINDING_INVALID",
                    format!(
                        "causal view {} has an inconsistent producing/LensId/LensSpec/extractor binding",
                        view.view_id
                    ),
                    "every producing view must bind all three physical identities; metadata-only candidates must bind none",
                ));
            }
        }
        if view.source_fields.is_empty() {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!("causal view {} has no source fields", view.view_id),
                "bind every view to at least one exact source field",
            ));
        }
        for source in &view.source_fields {
            validate_identity("source_field", source)?;
        }
        let expected_per_row_bytes = view.output.dim.checked_mul(4).ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
                format!(
                    "causal view {} output dim overflows u32 bytes",
                    view.view_id
                ),
                "bound the immutable output dimension so dim*4 fits u32",
            )
        })?;
        if view.output.dim == 0
            || view.per_row_worst_case_bytes != expected_per_row_bytes
            || !view.pre_trigger_only
            || view.estimator_compatibility.is_empty()
            || view
                .estimator_compatibility
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != view.estimator_compatibility.len()
            || view.estimator_compatibility != estimator_compatibility(view.family)
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_TRANSFORM_CONTRACT_INVALID",
                format!(
                    "causal view {} has invalid output/pre-trigger/row-byte/estimator metadata",
                    view.view_id
                ),
                "publish the exact bounded CPU transform, dense output contract, pre_trigger_only seal, dim*4 byte ceiling, and canonical estimator compatibility",
            ));
        }
        let expected_transform = if view.producing {
            let lens_name = view.lens_name.as_deref().ok_or_else(|| {
                registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_TRANSFORM_CONTRACT_INVALID",
                    format!("producing view {} has no lens name", view.view_id),
                    "bind the physical transform to its exact frozen lens identity",
                )
            })?;
            let spec = ACTION_PHYSICAL_CAUSAL_VIEW_SPECS
                .iter()
                .find(|spec| spec.lens_name == lens_name)
                .ok_or_else(|| {
                    registry_error(
                        "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_TRANSFORM_CONTRACT_INVALID",
                        format!(
                            "producing view {} names unknown lens {lens_name}",
                            view.view_id
                        ),
                        "bind the physical transform to one declared compact causal lens",
                    )
                })?;
            physical_transform_contract(spec.transform)
        } else {
            candidate_transform_contract(view.family)
        };
        if view.transform_id != expected_transform.0
            || view.transform != expected_transform.1
            || view.output != expected_transform.2
            || view.cpu_cost_class != expected_transform.3
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_TRANSFORM_CONTRACT_INVALID",
                format!("causal view {} transform metadata drifted", view.view_id),
                "restore the immutable typed transform id, parameterization, output shape, and CPU cost class",
            ));
        }
        *parent_counts.entry(&view.parent_atom).or_default() += 1;
        if !parent_families
            .entry(&view.parent_atom)
            .or_default()
            .insert(view.family.code())
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_CATALOG_INVALID",
                format!(
                    "causal atom {} repeats family {}",
                    view.parent_atom,
                    view.family.code()
                ),
                "declare each stable causal-view family at most once per atomic parent",
            ));
        }
        if view.producing {
            *parent_producers.entry(&view.parent_atom).or_default() += 1;
        }
        if view.lifecycle == SynapseCalyxCausalViewLifecycle::ServingCodeFrozen {
            *parent_serving.entry(&view.parent_atom).or_default() += 1;
        }
        if view.equivalence_owner {
            *parent_owners.entry(&view.parent_atom).or_default() += 1;
        }
        match (view.producing, view.slot, &view.lens_name, view.lifecycle) {
            (true, Some(_), Some(_), SynapseCalyxCausalViewLifecycle::ServingCodeFrozen)
                if view.equivalence_owner && view.refusal.is_none() => {}
            (true, Some(_), Some(_), SynapseCalyxCausalViewLifecycle::ParkedUnderpowered)
                if view.equivalence_owner && view.refusal.is_some() => {}
            (false, None, None, SynapseCalyxCausalViewLifecycle::ParkedCandidate)
                if !view.equivalence_owner && view.refusal.is_some() => {}
            _ => {
                return Err(registry_error(
                    "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_LIFECYCLE_INVALID",
                    format!(
                        "causal view {} has an inconsistent producing/slot/lens/owner/lifecycle/refusal contract",
                        view.view_id
                    ),
                    "physical primaries require a slot and lens; metadata candidates require neither; every non-serving view requires an explicit refusal",
                ));
            }
        }
    }
    for (parent, count) in &parent_counts {
        if !(SYNAPSE_CAUSAL_VIEW_REGISTRY_MIN_VIEWS_PER_PARENT
            ..=SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_VIEWS_PER_PARENT)
            .contains(count)
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PARENT_CARDINALITY_INVALID",
                format!(
                    "causal parent {parent} declares {count} view(s); required range is {SYNAPSE_CAUSAL_VIEW_REGISTRY_MIN_VIEWS_PER_PARENT}..={SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_VIEWS_PER_PARENT} per parent"
                ),
                "publish five to ten frozen independently measurable CPU views for the parent atom",
            ));
        }
        let producers = parent_producers.get(parent).copied().unwrap_or_default();
        let serving = parent_serving.get(parent).copied().unwrap_or_default();
        let owners = parent_owners.get(parent).copied().unwrap_or_default();
        if producers != 1 || serving > 1 || owners != 1 {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EQUIVALENCE_OWNER_INVALID",
                format!(
                    "causal atom {parent} has producers={producers}, serving={serving}, equivalence_owners={owners}; required producers=1, serving<=1, owners=1"
                ),
                "keep exactly one frozen producing/equivalence-owning primary per atom and leave alternative families as non-producing parked metadata",
            ));
        }
    }
    let expected_parents = ACTION_PHYSICAL_CAUSAL_VIEW_SPECS
        .iter()
        .map(|spec| spec.parent_atom)
        .collect::<BTreeSet<_>>();
    let actual_parents = parent_counts.keys().copied().collect::<BTreeSet<_>>();
    if actual_parents != expected_parents {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_UNDERPOPULATED",
            format!(
                "causal-view parent roster is {actual_parents:?}; exact immutable roster is {expected_parents:?}"
            ),
            "republish the complete eleven-parent compact action causal-view catalog; partial or foreign parents are never accepted",
        ));
    }
    for spec in ACTION_PHYSICAL_CAUSAL_VIEW_SPECS {
        let producing = catalog
            .iter()
            .filter(|view| view.producing && view.parent_atom == spec.parent_atom)
            .collect::<Vec<_>>();
        if producing.len() != 1 {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_ROSTER_INVALID",
                format!(
                    "causal parent {} has {} producing contracts",
                    spec.parent_atom,
                    producing.len()
                ),
                "restore exactly one immutable producing contract for every declared atomic parent",
            ));
        }
        let view = producing[0];
        let expected_lifecycle = if spec.slot == RESOURCE_HEADROOM_SLOT {
            SynapseCalyxCausalViewLifecycle::ParkedUnderpowered
        } else {
            SynapseCalyxCausalViewLifecycle::ServingCodeFrozen
        };
        if view.slot != Some(spec.slot)
            || view.lens_name.as_deref() != Some(spec.lens_name)
            || view.family != spec.primary_family
            || view.lifecycle != expected_lifecycle
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_PHYSICAL_ROSTER_INVALID",
                format!(
                    "causal parent {} producing contract is slot={:?} lens={:?} family={:?} lifecycle={:?}; expected slot={} lens={} family={:?} lifecycle={expected_lifecycle:?}",
                    spec.parent_atom,
                    view.slot,
                    view.lens_name,
                    view.family,
                    view.lifecycle,
                    spec.slot,
                    spec.lens_name,
                    spec.primary_family,
                ),
                "preserve the row and republish the exact immutable physical causal-view roster",
            ));
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "readback validation binds catalog, coverage, power, association policy, resource accounting, and ledger identity"
)]
fn validate_registry(
    registry: &SynapseCalyxCausalViewRegistry,
    expected_scope: &SynapseCalyxCausalViewRegistryScope,
) -> Result<(), SynapseCalyxError> {
    if registry.schema_version != SYNAPSE_CAUSAL_VIEW_REGISTRY_SCHEMA_VERSION {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SCHEMA_MISMATCH",
            format!(
                "causal-view Registry schema {} != supported {}",
                registry.schema_version, SYNAPSE_CAUSAL_VIEW_REGISTRY_SCHEMA_VERSION
            ),
            "republish the Registry with this binary; unknown schemas are never guessed",
        ));
    }
    if registry.generation == 0 || registry.scope != *expected_scope {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SCOPE_MISMATCH",
            "causal-view Registry row does not own the requested scope or has generation zero",
            "quarantine the mis-keyed Registry row and republish the exact scope",
        ));
    }
    let requested = registry.measurement_contract.requested_max_records;
    let effective = registry.measurement_contract.effective_max_records;
    if registry.source_panel_content_seq == 0
        || registry.source_panel_content_seq > registry.assembled_at_seq
        || registry.source_anchors_cf_last_commit_seq > registry.assembled_at_seq
        || !(1..=crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS).contains(&requested)
        || effective != requested.min(crate::SYNAPSE_ENSEMBLE_MAX_RECORDS)
        || registry.measurement_contract.required_record_slots
            != [SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT]
        || registry.measurement_contract.estimator != ENSEMBLE_CARD_PID_METHOD
        || registry.evidence.scan_limit_reached != (registry.evidence.records_scanned == effective)
        || registry.evidence.census_complete != (registry.evidence.records_scanned < effective)
        || registry.evidence.records_scanned > effective
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_MEASUREMENT_CONTRACT_INVALID",
            format!(
                "assembled_at_seq={} source_panel_content_seq={} source_anchors_cf_frontier=({}, {}) estimator={:?} requested_max_records={} effective_max_records={} records_scanned={} scan_limit_reached={} census_complete={}",
                registry.assembled_at_seq,
                registry.source_panel_content_seq,
                registry.source_anchors_cf_last_commit_seq,
                registry.source_anchors_cf_out_of_band_epoch,
                registry.measurement_contract.estimator,
                requested,
                effective,
                registry.evidence.records_scanned,
                registry.evidence.scan_limit_reached,
                registry.evidence.census_complete,
            ),
            "republish with the frozen estimator and required complete-cause record slot, requested max_records in range, effective=min(requested,1000), exact panel/Anchors frontiers no later than assembly, and explicit bounded-scan completeness",
        ));
    }
    let expected_scope_hash = scope_sha256(expected_scope)?;
    if registry.logical_scope_sha256 != expected_scope_hash {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SCOPE_MISMATCH",
            format!(
                "causal-view Registry scope hash {} != expected {expected_scope_hash}",
                registry.logical_scope_sha256
            ),
            "quarantine the colliding or mis-keyed Registry row and republish the exact scope",
        ));
    }
    validate_catalog(&registry.catalog)?;
    let expected_parked_underpowered = registry
        .catalog
        .iter()
        .filter(|contract| {
            contract.producing
                && contract.lifecycle == SynapseCalyxCausalViewLifecycle::ParkedUnderpowered
        })
        .filter_map(|contract| contract.slot)
        .collect::<Vec<_>>();
    let caller_excluded = registry
        .measurement_contract
        .caller_excluded_slots
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let caller_excluded_serving = registry
        .catalog
        .iter()
        .filter(|contract| {
            contract.producing
                && contract.lifecycle == SynapseCalyxCausalViewLifecycle::ServingCodeFrozen
        })
        .filter_map(|contract| contract.slot)
        .filter(|slot| caller_excluded.contains(slot))
        .collect::<Vec<_>>();
    if !caller_excluded_serving.is_empty() {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SERVING_VIEW_EXCLUDED",
            format!(
                "stored measurement contract excludes immutable serving causal-view slots {caller_excluded_serving:?}"
            ),
            "quarantine the inconsistent Registry row and republish without excluding ServingCodeFrozen views",
        ));
    }
    let caller_excluded_predictor = crate::action_validation::ACTION_CAUSAL_PREDICTOR_SLOTS
        .iter()
        .copied()
        .filter(|slot| caller_excluded.contains(slot))
        .collect::<Vec<_>>();
    let declared_slot_ids = registry
        .evidence
        .declared_slot_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let caller_excluded_undeclared = caller_excluded
        .difference(&declared_slot_ids)
        .copied()
        .collect::<Vec<_>>();
    if !caller_excluded_predictor.is_empty() || !caller_excluded_undeclared.is_empty() {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_MEASUREMENT_CONTRACT_INVALID",
            format!(
                "stored caller exclusions contain predictor slots {caller_excluded_predictor:?} or undeclared slots {caller_excluded_undeclared:?}"
            ),
            "quarantine the inconsistent Registry row and republish with only real declared non-predictor exclusions",
        ));
    }
    if registry
        .measurement_contract
        .registry_parked_underpowered_slots
        != expected_parked_underpowered
        || expected_parked_underpowered
            .iter()
            .any(|slot| registry.evidence.estimable_slots.contains(slot))
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_LIFECYCLE_INVALID",
            "parked-underpowered catalog slots do not match the assay exclusion contract, or entered the estimator",
            "derive registry_parked_underpowered_slots from the immutable catalog and exclude them before the ensemble assay",
        ));
    }
    let strictly_sorted_unique = |slots: &[u16]| {
        slots
            .windows(2)
            .all(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| a < b))
    };
    let declared = registry
        .evidence
        .declared_slot_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let available = registry
        .evidence
        .physically_available_slots
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let estimable = registry
        .evidence
        .estimable_slots
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if registry.evidence.declared_slot_ids.len() != registry.evidence.declared_slots
        || !strictly_sorted_unique(&registry.evidence.declared_slot_ids)
        || !strictly_sorted_unique(&registry.evidence.physically_available_slots)
        || !strictly_sorted_unique(&registry.evidence.estimable_slots)
        || !available.is_subset(&declared)
        || !estimable.is_subset(&available)
        || !available.contains(&SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT)
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_SLOT_ROSTERS_INVALID",
            format!(
                "declared_count={} declared_ids={:?} physically_available={:?} estimable={:?} required_record_slot={SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT}",
                registry.evidence.declared_slots,
                registry.evidence.declared_slot_ids,
                registry.evidence.physically_available_slots,
                registry.evidence.estimable_slots,
            ),
            "republish from one cohort scan with sorted unique rosters satisfying estimable subset physically_available subset declared and the writer-sealed required slot physically present",
        ));
    }
    let producing = registry
        .catalog
        .iter()
        .filter(|contract| contract.producing)
        .collect::<Vec<_>>();
    if registry.evidence.views.len() != producing.len() {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EVIDENCE_INCOMPLETE",
            format!(
                "causal-view Registry has {} producing contracts but {} physical evidence rows",
                producing.len(),
                registry.evidence.views.len()
            ),
            "republish exactly one physical evidence row per producing primary; metadata-only candidates never receive invented measurements",
        ));
    }
    for (contract, result) in producing.iter().zip(&registry.evidence.views) {
        if contract.slot != Some(result.slot)
            || contract.lens_name.as_deref() != Some(result.lens_name.as_str())
            || result.anchored_records_carried > registry.evidence.anchored_records
            || !result.anchored_coverage.is_finite()
            || !(0.0..=1.0).contains(&result.anchored_coverage)
        {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_EVIDENCE_INCOMPLETE",
                format!(
                    "causal-view evidence slot/name/coverage ({}, {}, {}/{}) does not match producing contract {:?}/{:?}",
                    result.slot,
                    result.lens_name,
                    result.anchored_records_carried,
                    registry.evidence.anchored_records,
                    contract.slot,
                    contract.lens_name,
                ),
                "republish from the single-pass physical-slot coverage and exact producing catalog",
            ));
        }
        let is_available = available.contains(&result.slot);
        let is_estimable = estimable.contains(&result.slot);
        let measurement_coherent =
            match &result.measurement {
                SynapseCalyxCausalViewMeasurement::Measured { .. } => is_available && is_estimable,
                SynapseCalyxCausalViewMeasurement::AvailableUnmeasured {
                    code,
                    analytical_incremental_bits,
                    ..
                } => is_available && !is_estimable && match code {
                    SynapseCalyxCausalViewAvailableUnmeasuredCode::ObservedCohortConstant => {
                        *analytical_incremental_bits == Some(0.0)
                    }
                    SynapseCalyxCausalViewAvailableUnmeasuredCode::DegenerateRedundancySketch => {
                        analytical_incremental_bits.is_none()
                    }
                },
                SynapseCalyxCausalViewMeasurement::Excluded { code, .. } => {
                    !is_estimable
                        && match code {
                            SynapseCalyxCausalViewExclusionCode::ParkedUnderpowered => true,
                            SynapseCalyxCausalViewExclusionCode::WithheldByCaller => is_available,
                            SynapseCalyxCausalViewExclusionCode::MissingAnchoredCoverage
                            | SynapseCalyxCausalViewExclusionCode::UnusableRepresentation
                            | SynapseCalyxCausalViewExclusionCode::RaggedColumn => !is_available,
                        }
                }
            };
        if !measurement_coherent {
            return Err(registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_VIEW_STATE_INVALID",
                format!(
                    "slot {} measurement={:?} physically_available={} estimable={}",
                    result.slot, result.measurement, is_available, is_estimable
                ),
                "republish each physical view with a state coherent with the typed availability and estimator rosters; structural zero is exact 0 bits, sketch degeneracy is never assigned a bit value",
            ));
        }
    }
    let complete_observed = registry
        .evidence
        .views
        .iter()
        .map(|view| view.anchored_records_carried)
        .min()
        .unwrap_or_default();
    let expected_power = if complete_observed < SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES {
        SynapseCalyxCausalViewSelectionPowerState::Underpowered
    } else {
        SynapseCalyxCausalViewSelectionPowerState::Powered
    };
    if registry.evidence.observed_sample_count != complete_observed
        || registry.evidence.required_sample_count != SYNAPSE_CAUSAL_VIEW_SELECTION_REQUIRED_SAMPLES
        || registry.evidence.selection_power_state != expected_power
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_POWER_STATE_INVALID",
            "causal-view selection-power fields do not match the complete anchored cohort and frozen 269-sample threshold",
            "republish the Registry from the exact anchored cohort; never label feature stable rank as effective sample size",
        ));
    }
    let expected_association_policy = SynapseCalyxCausalViewAssociationPolicy {
        scope: SynapseCalyxCausalViewAssociationScope::WithinAtomOnly,
        views_as_stream_nodes: false,
        sibling_pairs_generated: 0,
    };
    if registry.association_policy != expected_association_policy {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_ASSOCIATION_SCOPE_INVALID",
            "causal-view Registry attempted to treat within-atom alternatives as stream nodes or generated sibling pairs",
            "restore within_atom_only scope, views_as_stream_nodes=false, and sibling_pairs_generated=0",
        ));
    }
    if registry.resource_accounting
        != causal_view_resource_accounting(
            &registry.catalog,
            registry.measurement_contract.effective_max_records,
        )?
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_ACCOUNTING_INVALID",
            "causal-view Registry resource accounting does not reproduce from its typed catalog",
            "republish the compact Registry: its artifact must contain zero vectors/matrices, its producing panel payload must reproduce from dim*4*max_records, and every runtime contract must remain CPU-only",
        ));
    }
    ensure_finite_values(
        "stored ensemble panel",
        &[
            registry.evidence.anchor_entropy_bits,
            registry.evidence.panel_bits,
            registry.evidence.panel_ci[0],
            registry.evidence.panel_ci[1],
            registry.evidence.effective_rank_features,
            registry.evidence.deficit_bits,
        ],
    )?;
    if registry.ledger_seq > 0
        && (registry.ledger_hash.len() != 64
            || !registry
                .ledger_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_LEDGER_REF_INVALID",
            "causal-view Registry ledger hash is not a lowercase 32-byte canonical ledger-hash hex value",
            "quarantine the row and republish it through the atomic Registry+Ledger writer",
        ));
    }
    Ok(())
}

fn causal_view_resource_accounting(
    catalog: &[SynapseCalyxCausalViewContract],
    max_records: usize,
) -> Result<SynapseCalyxCausalViewResourceAccounting, SynapseCalyxError> {
    let producing_f32_payload_bytes_per_record = catalog
        .iter()
        .filter(|view| view.producing)
        .map(|view| u64::from(view.per_row_worst_case_bytes))
        .sum::<u64>();
    let max_records_u64 = u64::try_from(max_records).map_err(|error| {
        registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
            format!("convert causal-view max_records={max_records} to u64: {error}"),
            "keep max_records inside the declared bounded assay contract",
        )
    })?;
    let producing_f32_payload_bytes_at_max_records = producing_f32_payload_bytes_per_record
        .checked_mul(max_records_u64)
        .ok_or_else(|| {
            registry_error(
                "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
                format!(
                    "causal-view payload accounting overflow: per_record={producing_f32_payload_bytes_per_record} max_records={max_records_u64}"
                ),
                "reduce the bounded panel dimensions or max_records; resource accounting never saturates or wraps",
            )
        })?;
    Ok(SynapseCalyxCausalViewResourceAccounting {
        registry_value_ceiling_bytes: SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES,
        contract_count: catalog.len(),
        producing_contract_count: catalog.iter().filter(|view| view.producing).count(),
        serving_contract_count: catalog
            .iter()
            .filter(|view| view.lifecycle == SynapseCalyxCausalViewLifecycle::ServingCodeFrozen)
            .count(),
        parked_underpowered_contract_count: catalog
            .iter()
            .filter(|view| view.lifecycle == SynapseCalyxCausalViewLifecycle::ParkedUnderpowered)
            .count(),
        metadata_only_candidate_contract_count: catalog
            .iter()
            .filter(|view| !view.producing)
            .count(),
        cpu_runtime_contract_count: catalog
            .iter()
            .filter(|view| view.runtime == SynapseCalyxCausalViewRuntime::CpuDeterministic)
            .count(),
        gpu_runtime_contract_count: 0,
        registry_persisted_vector_bytes: 0,
        registry_persisted_matrix_bytes: 0,
        producing_f32_payload_bytes_per_record,
        producing_f32_payload_bytes_at_max_records,
        candidate_materialized_payload_bytes: 0,
    })
}

fn ledger_payload_bytes(
    registry: &SynapseCalyxCausalViewRegistry,
) -> Result<Vec<u8>, SynapseCalyxError> {
    serde_json::to_vec(&CausalViewRegistryLedgerPayload {
        registry_content_sha256: registry_content_sha256(registry)?,
        schema_version: registry.schema_version,
        generation: registry.generation,
        logical_scope_sha256: &registry.logical_scope_sha256,
        measurement_contract_sha256: &registry.measurement_contract_sha256,
        catalog_sha256: &registry.catalog_sha256,
        evidence_sha256: &registry.evidence_sha256,
        panel_version: registry.scope.panel_version,
        source_panel_content_seq: registry.source_panel_content_seq,
        source_anchors_cf_last_commit_seq: registry.source_anchors_cf_last_commit_seq,
        source_anchors_cf_out_of_band_epoch: registry.source_anchors_cf_out_of_band_epoch,
        anchor_kind: &registry.scope.anchor_kind,
        view_count: registry.catalog.len(),
        anchored_records: registry.evidence.anchored_records,
    })
    .map_err(|error| encode_error("causal-view Registry ledger payload", &error))
}

fn registry_content_sha256(
    registry: &SynapseCalyxCausalViewRegistry,
) -> Result<String, SynapseCalyxError> {
    let mut content = registry.clone();
    content.ledger_seq = 0;
    content.ledger_hash.clear();
    let bytes = serde_json::to_vec(&content)
        .map_err(|error| encode_error("ledger-bound causal-view Registry content", &error))?;
    Ok(crate::sha256_hex(&bytes))
}

fn preflight_stored_size(draft: &SynapseCalyxCausalViewRegistry) -> Result<(), SynapseCalyxError> {
    let mut largest = draft.clone();
    largest.ledger_seq = u64::MAX;
    largest.ledger_hash = "f".repeat(64);
    let registry_bytes = serde_json::to_vec(&largest)
        .map_err(|error| encode_error("causal-view Registry preflight", &error))?;
    let stored = StoredCausalViewRegistry {
        registry: largest,
        registry_sha256: crate::sha256_hex(&registry_bytes),
    };
    let bytes = serde_json::to_vec(&stored)
        .map_err(|error| encode_error("stored causal-view Registry preflight", &error))?;
    if bytes.len() > SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_RESOURCE_BUDGET_EXCEEDED",
            format!(
                "causal-view Registry requires {} bytes; ceiling is {} bytes",
                bytes.len(),
                SYNAPSE_CAUSAL_VIEW_REGISTRY_MAX_BYTES
            ),
            "reduce contract prose without removing views or evidence; vectors and matrices must remain outside Registry",
        ));
    }
    Ok(())
}

fn ensure_finite_values(label: &str, values: &[f32]) -> Result<(), SynapseCalyxError> {
    if let Some((index, value)) = values
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
    {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_NON_FINITE",
            format!("causal-view {label} scalar index {index} is non-finite ({value})"),
            "preserve the Assay evidence and repair the estimator producing the non-finite scalar",
        ));
    }
    Ok(())
}

fn scope_wire_bytes(
    scope: &SynapseCalyxCausalViewRegistryScope,
) -> Result<Vec<u8>, SynapseCalyxError> {
    validate_scope(scope)?;
    let mut bytes = Vec::new();
    append_scope_part(&mut bytes, REGISTRY_SCOPE_TAG);
    append_scope_part(&mut bytes, &scope.panel_version.to_be_bytes());
    append_scope_part(&mut bytes, scope.corpus_shard.as_bytes());
    append_scope_part(&mut bytes, scope.anchor_kind.as_bytes());
    Ok(bytes)
}

fn scope_sha256(scope: &SynapseCalyxCausalViewRegistryScope) -> Result<String, SynapseCalyxError> {
    Ok(crate::sha256_hex(&scope_wire_bytes(scope)?))
}

fn registry_key(scope: &SynapseCalyxCausalViewRegistryScope) -> Result<Vec<u8>, SynapseCalyxError> {
    let digest = sha2::Sha256::digest(scope_wire_bytes(scope)?);
    let mut key = Vec::with_capacity(REGISTRY_KEY_PREFIX.len() + 4 + 16);
    key.extend_from_slice(REGISTRY_KEY_PREFIX);
    key.extend_from_slice(&scope.panel_version.to_be_bytes());
    key.extend_from_slice(&digest[..16]);
    Ok(key)
}

fn append_scope_part(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn typed_sha256<T: Serialize>(label: &str, value: &T) -> Result<String, SynapseCalyxError> {
    let bytes = serde_json::to_vec(value).map_err(|error| encode_error(label, &error))?;
    Ok(crate::sha256_hex(&bytes))
}

fn verify_typed_hash<T: Serialize>(
    label: &str,
    value: &T,
    expected: &str,
) -> Result<(), SynapseCalyxError> {
    let actual = typed_sha256(label, value)?;
    if actual != expected {
        return Err(registry_error(
            "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_INTEGRITY_FAILED",
            format!("causal-view Registry {label} hash {actual} != stored {expected}"),
            "quarantine the tampered Registry row and republish it from frozen metadata and measured Assay evidence",
        ));
    }
    Ok(())
}

fn encode_error(label: &str, error: &serde_json::Error) -> SynapseCalyxError {
    registry_error(
        "SYNAPSE_CALYX_CAUSAL_VIEW_REGISTRY_ENCODE_FAILED",
        format!("encode {label}: {error}"),
        "repair the typed finite Registry value; no alternate serialization is accepted",
    )
}

fn registry_error(
    code: &'static str,
    message: impl Into<String>,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message, remediation)
}
