//! Persisted, read-only health snapshot for the action Oracle readiness gate.
//!
//! The two autonomy-critical tiers — `GoodhartDefended` and `MistakeClosed` —
//! are never computed here. They are *admitted* here: readiness reads the
//! held-out validation report that `oracle_validate` physically persisted and
//! decides whether that report is still allowed to authorize autonomy.
//!
//! Admission is a conjunction of named predicates over the physical evidence
//! (issue #2017). Each predicate reports what it measured, the row it measured
//! it from, the threshold it was compared against, and the remediation for the
//! failure — and the whole admission log is written into the persisted Anneal
//! readiness row so it can be read back and checked against its source report
//! without rerunning anything.
//!
//! Every predicate fails closed. Absent, corrupt, out-of-scope, unbound,
//! superseded, rebound, or expired evidence all refuse; none of them can be
//! bypassed, and none of them silently degrade into a pass.

use calyx_aster::cf::ColumnFamily;
use calyx_core::Panel;
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use calyx_oracle::{DomainId, SuperIntelReport, Tier, TierResult};
use num_traits::ToPrimitive as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::action_validation::{
    ACTION_CAUSAL_POPULATION_CONTRACT, ACTION_CAUSAL_PREDICTOR, ACTION_CAUSAL_PREDICTOR_SLOTS,
    ACTION_CAUSAL_REGISTRY_SERVING_SLOTS, ACTION_PANEL_VERSION, ACTION_VALIDATION_KEY,
    ACTION_VALIDATION_SCHEMA_VERSION, MIN_HELD_OUT_RECORDS, action_causal_predictor_sha256,
    action_validation_ledger_payload_sha256,
};
use crate::{
    SYNAPSE_INTELLIGENCE_MAX_RECORDS, SYNAPSE_KSG_DEFAULT_K, SynapseCalyxActionValidationEvidence,
    SynapseCalyxAssayParams, SynapseCalyxError, SynapseCalyxSlotBitsState, SynapseCalyxVault,
};

const ACTION_DOMAIN: &str = "synapse.action";
const ACTION_CONTENT_SLOT: u16 = 50;
const ACTION_ANCHOR_KIND: &str = "reward";
/// Immutable readiness row generation. `v7` ledger-authenticates the complete
/// readiness content, binds the exact full predictor plus estimator-backed
/// sufficiency rosters and source-CF signals, and requires the canonical
/// admission predicate roster.
/// exact typed slot set plus both the present and missing populations for the
/// collection-only resource-prestate cause. Older rows remain historical and
/// are never inferred, upgraded, or served as current.
const READINESS_KEY: &[u8] = b"oracle-readiness/v7/synapse.action";
const READINESS_SCHEMA_VERSION: u32 = 7;
const EVIDENCE_CF: &str = "AnnealReport";
const LEDGER_SOURCE: &str = "Ledger/anneal";
const GUARD_SOURCE: &str = "Guard/profile\\0panel\\0<panel_version>";
const CORPUS_SOURCE: &str = "Base/panel=2260001,oracle.domain=synapse.action";
const READY_ADMISSION_PREDICATES: [&str; 13] = [
    "evidence_row_present",
    "evidence_integrity",
    "evidence_scope",
    "evidence_population_accounted",
    "evidence_resource_population_accounted",
    "evidence_complete",
    "evidence_ledger_bound",
    "evidence_corpus_fresh",
    "evidence_causal_registry_fresh",
    "evidence_guard_fresh",
    "evidence_lease",
    "goodhart_defended",
    "mistake_closed",
];

/// Freshness lease on held-out action evidence, in milliseconds (7 days).
///
/// The corpus binding below is an *exact* version precondition: the moment a
/// single new terminal action outcome lands, the evidence stops matching and
/// readiness refuses. So this lease only governs the quiescent case — a vault
/// that has recorded no new action outcome at all for a week. That silence is
/// not evidence that autonomy is still safe, so a passing report is not
/// allowed to authorize autonomy indefinitely on the strength of its age.
pub const ACTION_EVIDENCE_LEASE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// One named admission predicate and everything needed to audit its verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxReadinessPredicate {
    /// Stable predicate identifier, e.g. `evidence_corpus_fresh`.
    pub predicate: String,
    pub passed: bool,
    /// Structured failure code; `None` exactly when the predicate passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Physical source this predicate read.
    pub source: String,
    /// What was actually observed.
    pub measured: String,
    /// What was required for the predicate to hold.
    pub expected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

/// Provenance of the held-out report the two autonomy tiers were derived from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxReadinessEvidence {
    pub source_cf: String,
    pub source_key: String,
    pub schema_version: u32,
    /// SHA-256 revision of the exact physical evidence row that was read.
    pub row_revision_sha256: String,
    pub measured_at_seq: u64,
    /// Exact action-panel content watermark measured by validation.
    pub panel_content_seq: u64,
    pub ledger_seq: u64,
    pub ledger_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_ts_ms: Option<u64>,
    /// Age of the evidence at measurement time, from its own ledger entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    pub lease_ms: u64,
    pub causal_population_contract: String,
    pub source_reward_record_count: usize,
    pub action_record_count: usize,
    pub action_corpus_sha256: String,
    pub excluded_incomplete_causal_records: usize,
    pub excluded_incomplete_causal_sha256: String,
    pub excluded_incomplete_causal_sample: Vec<String>,
    /// Exact serving predictor contract admitted by this readiness snapshot.
    pub predictor: String,
    pub predictor_slots: Vec<u16>,
    pub predictor_sha256: String,
    pub predictor_artifact_blob_id: String,
    pub predictor_artifact_blake3: String,
    /// Collection-only slot 136 coverage. Missing identities are deliberately
    /// retained and hash-bound; zero coverage does not authorize serving this
    /// underpowered cause and does not invalidate the slot-125 population.
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
    pub guard_profile_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goodhart_in_region_frac: Option<f64>,
    pub goodhart_violations: usize,
    pub mistake_regression_count: usize,
    pub mistake_regression_evaluated: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxReadinessSnapshot {
    pub schema_version: u32,
    /// Exact semantic domain this evidence measures.
    pub domain: String,
    /// Frozen panel generation whose slots, Guard, kernel, and held-out
    /// evidence produced this verdict.
    pub panel_version: u32,
    pub report: SuperIntelReport,
    pub measured_at_seq: u64,
    pub persisted_at_seq: Option<u64>,
    pub row_revision_sha256: String,
    pub content_sha256: String,
    pub ledger_seq: u64,
    pub ledger_hash: String,
    pub source_signals: SynapseCalyxReadinessSourceSignals,
    /// Exact nonconstant, estimator-backed predictor roster used by the panel
    /// sufficiency tier. The full frozen predictor roster remains bound by the
    /// validation evidence; valid constant lanes are not mislabeled missing.
    pub sufficiency_slots: Vec<u16>,
    /// Full-predictor complement proved exact-constant over the same canonical
    /// grounded cohort. These slots remain required serving inputs but have no
    /// fabricated Lens Assay row.
    pub structural_zero_slots: Vec<u16>,
    pub sufficiency_slots_sha256: String,
    /// The report the autonomy tiers were admitted from; `None` when no
    /// admissible evidence row could be read at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SynapseCalyxReadinessEvidence>,
    /// Every admission predicate in evaluation order, passing and failing.
    pub evidence_admission: Vec<SynapseCalyxReadinessPredicate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxReadinessSourceSignals {
    pub anchors: (u64, u64),
    pub recurrence: (u64, u64),
    pub assay: (u64, u64),
    pub kernel: (u64, u64),
    pub guard: (u64, u64),
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredReadinessContent {
    schema_version: u32,
    domain: String,
    panel_version: u32,
    report: SuperIntelReport,
    measured_at_seq: u64,
    source_signals: SynapseCalyxReadinessSourceSignals,
    sufficiency_slots: Vec<u16>,
    structural_zero_slots: Vec<u16>,
    sufficiency_slots_sha256: String,
    #[serde(default)]
    evidence: Option<SynapseCalyxReadinessEvidence>,
    #[serde(default)]
    evidence_admission: Vec<SynapseCalyxReadinessPredicate>,
}

#[derive(Serialize, Deserialize)]
struct StoredReadinessSnapshot {
    content: StoredReadinessContent,
    content_sha256: String,
    ledger_seq: u64,
    ledger_hash: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ReadinessLedgerPayload<'a> {
    tag: &'static str,
    content_sha256: &'a str,
    domain: &'a str,
    panel_version: u32,
    measured_at_seq: u64,
}

/// Verifies that a persisted action-readiness snapshot is currently allowed to
/// authorize serving.
///
/// # Errors
///
/// Returns a typed refusal when any readiness tier, evidence-admission
/// predicate, sufficiency partition, or authenticated roster binding is absent
/// or stale.
pub fn ensure_action_readiness_serving_admitted(
    snapshot: &SynapseCalyxReadinessSnapshot,
) -> Result<(), SynapseCalyxError> {
    let sufficiency_roster_valid =
        sufficiency_partition_valid(&snapshot.sufficiency_slots, &snapshot.structural_zero_slots)
            && snapshot.sufficiency_slots_sha256
                == sufficiency_slots_sha256(
                    &snapshot.sufficiency_slots,
                    &snapshot.structural_zero_slots,
                );
    let tier_roster_valid = snapshot.report.tiers.len() == Tier::ORDER.len()
        && snapshot
            .report
            .tiers
            .iter()
            .zip(Tier::ORDER)
            .all(|(observed, expected)| observed.tier == expected && observed.passed)
        && snapshot.report.overall
        && snapshot.report.failing_tier.is_none()
        && snapshot.report.cheapest_fix.is_none();
    let predicate_roster_valid = snapshot.evidence_admission.len()
        == READY_ADMISSION_PREDICATES.len()
        && snapshot
            .evidence_admission
            .iter()
            .zip(READY_ADMISSION_PREDICATES)
            .all(|(observed, expected)| {
                observed.predicate == expected
                    && observed.passed
                    && observed.code.is_none()
                    && observed.remediation.is_none()
            });
    if !tier_roster_valid
        || !predicate_roster_valid
        || !sufficiency_roster_valid
        || snapshot.evidence.is_none()
    {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_TYPED_PREDICTION_NOT_READY",
            format!(
                "authenticated readiness does not carry the exact serving roster: overall={} tiers={:?} predicates={:?} sufficiency_slots={:?} structural_zero_slots={:?} sufficiency_roster_valid={} evidence_present={}",
                snapshot.report.overall,
                snapshot
                    .report
                    .tiers
                    .iter()
                    .map(|tier| (tier.tier, tier.passed))
                    .collect::<Vec<_>>(),
                snapshot
                    .evidence_admission
                    .iter()
                    .map(|predicate| (predicate.predicate.as_str(), predicate.passed))
                    .collect::<Vec<_>>(),
                snapshot.sufficiency_slots,
                snapshot.structural_zero_slots,
                sufficiency_roster_valid,
                snapshot.evidence.is_some(),
            ),
            "run oracle_validate and oracle_readiness until the exact six tiers and thirteen named admission predicates all pass",
        ));
    }
    Ok(())
}

/// A refused admission: the failing predicate's code and its remediation,
/// already recorded in the admission log.
struct AdmissionRefusal {
    code: String,
    detail: String,
    remediation: String,
}

impl AdmissionRefusal {
    /// The `cheapest_fix` text carried on both refused tiers. It names the
    /// failing code, what was actually observed, and the exact next action, so
    /// a caller that only reads `report.cheapest_fix` still learns all three.
    fn tier_fix(&self) -> String {
        format!(
            "{}: {}; remediation: {}",
            self.code, self.detail, self.remediation
        )
    }
}

/// Records a satisfied predicate.
fn admit(
    log: &mut Vec<SynapseCalyxReadinessPredicate>,
    predicate: &str,
    source: impl Into<String>,
    measured: impl Into<String>,
    expected: impl Into<String>,
) {
    log.push(SynapseCalyxReadinessPredicate {
        predicate: predicate.to_owned(),
        passed: true,
        code: None,
        source: source.into(),
        measured: measured.into(),
        expected: expected.into(),
        remediation: None,
    });
}

/// Records a refused predicate and produces the refusal both tiers carry.
fn refuse(
    log: &mut Vec<SynapseCalyxReadinessPredicate>,
    predicate: &str,
    source: impl Into<String>,
    code: impl Into<String>,
    measured: impl Into<String>,
    expected: impl Into<String>,
    remediation: impl Into<String>,
) -> AdmissionRefusal {
    let code = code.into();
    let measured = measured.into();
    let remediation = remediation.into();
    log.push(SynapseCalyxReadinessPredicate {
        predicate: predicate.to_owned(),
        passed: false,
        code: Some(code.clone()),
        source: source.into(),
        measured: measured.clone(),
        expected: expected.into(),
        remediation: Some(remediation.clone()),
    });
    AdmissionRefusal {
        code,
        detail: measured,
        remediation,
    }
}

fn evidence_key_name() -> String {
    String::from_utf8_lossy(ACTION_VALIDATION_KEY).into_owned()
}

fn evidence_source() -> String {
    format!("{EVIDENCE_CF}/{}", evidence_key_name())
}

fn readiness_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

fn sufficiency_slots_sha256(informative_slots: &[u16], structural_zero_slots: &[u16]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-action-readiness-sufficiency-slots-v1");
    hasher.update(
        u64::try_from(ACTION_CAUSAL_PREDICTOR_SLOTS.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for slot in ACTION_CAUSAL_PREDICTOR_SLOTS {
        hasher.update(slot.to_be_bytes());
    }
    hasher.update(
        u64::try_from(informative_slots.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for slot in informative_slots {
        hasher.update(slot.to_be_bytes());
    }
    hasher.update(
        u64::try_from(structural_zero_slots.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for slot in structural_zero_slots {
        hasher.update(slot.to_be_bytes());
    }
    readiness_hex(&hasher.finalize())
}

fn sufficiency_partition_valid(informative_slots: &[u16], structural_zero_slots: &[u16]) -> bool {
    let sorted_unique = |slots: &[u16]| {
        slots
            .windows(2)
            .all(|pair| pair.first().zip(pair.get(1)).is_some_and(|(a, b)| a < b))
    };
    if informative_slots.is_empty()
        || !sorted_unique(informative_slots)
        || !sorted_unique(structural_zero_slots)
    {
        return false;
    }
    let informative = informative_slots
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let structural_zero = structural_zero_slots
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let full = ACTION_CAUSAL_PREDICTOR_SLOTS
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    informative.is_disjoint(&structural_zero)
        && informative
            .union(&structural_zero)
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            == full
}

fn readiness_publication_stale(
    phase: &str,
    source: &str,
    expected: impl std::fmt::Display,
    actual: impl std::fmt::Display,
) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_READINESS_PUBLICATION_SOURCE_MOVED",
        format!(
            "readiness source moved {phase}: source={source} expected={expected} actual={actual}"
        ),
        "read the physical readiness row to determine whether publication crossed its commit boundary, preserve any stale committed row as historical evidence, and remeasure oracle_readiness against one stable validation/Registry/Guard/source generation",
    )
}

impl SynapseCalyxVault {
    fn action_readiness_source_signals(&self) -> SynapseCalyxReadinessSourceSignals {
        SynapseCalyxReadinessSourceSignals {
            anchors: self.cf_change_signal(ColumnFamily::Anchors),
            recurrence: self.cf_change_signal(ColumnFamily::Recurrence),
            assay: self.cf_change_signal(ColumnFamily::Assay),
            kernel: self.cf_change_signal(ColumnFamily::Kernel),
            guard: self.cf_change_signal(ColumnFamily::Guard),
        }
    }

    pub(crate) fn ensure_action_readiness_sources_current(
        &self,
        snapshot: &SynapseCalyxReadinessSnapshot,
    ) -> Result<(), SynapseCalyxError> {
        let current = self.action_readiness_source_signals();
        if current != snapshot.source_signals {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SOURCE_FRONTIER_MOVED",
                format!(
                    "persisted readiness source signals {:?} differ from current {:?}",
                    snapshot.source_signals, current
                ),
                "remeasure oracle_readiness after any grounded outcome, recurrence, Assay, Kernel, or Guard mutation",
            ));
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one authenticated freshness boundary must compare validation, Registry, Guard, and their physical ledger identities without partial success"
    )]
    fn ensure_admitted_action_evidence_binding_current(
        &self,
        expected: Option<&SynapseCalyxReadinessEvidence>,
        phase: &str,
    ) -> Result<(), SynapseCalyxError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let (validation, validation_row_revision_sha256) =
            self.read_action_validation_revisioned()?.ok_or_else(|| {
                readiness_publication_stale(
                    phase,
                    "admitted validation row",
                    expected.row_revision_sha256.clone(),
                    "absent".to_owned(),
                )
            })?;
        let validation_binding = (
            validation_row_revision_sha256.as_str(),
            validation.ledger_seq,
            validation.ledger_hash.as_str(),
        );
        let expected_validation_binding = (
            expected.row_revision_sha256.as_str(),
            expected.ledger_seq,
            expected.ledger_hash.as_str(),
        );
        if validation_binding != expected_validation_binding {
            return Err(readiness_publication_stale(
                phase,
                "admitted validation row revision/ledger",
                format!("{expected_validation_binding:?}"),
                format!("{validation_binding:?}"),
            ));
        }
        let validation_ledger = self.read_ledger_entry(validation.ledger_seq)?;
        let validation_ts_ms = validation_ledger.ts.ok_or_else(|| {
            readiness_publication_stale(
                phase,
                "admitted validation ledger timestamp",
                "present monotone timestamp".to_owned(),
                "absent".to_owned(),
            )
        })?;
        let now_ms = self.clock_now_ms()?;
        let evidence_age_ms = now_ms.checked_sub(validation_ts_ms).ok_or_else(|| {
            readiness_publication_stale(
                phase,
                "admitted validation ledger clock",
                format!("validation_ts_ms <= now_ms ({now_ms})"),
                format!("validation_ts_ms={validation_ts_ms}"),
            )
        })?;
        if evidence_age_ms > ACTION_EVIDENCE_LEASE_MS {
            return Err(readiness_publication_stale(
                phase,
                "admitted validation evidence lease",
                format!("age_ms <= {ACTION_EVIDENCE_LEASE_MS}"),
                format!("age_ms={evidence_age_ms}"),
            ));
        }

        let registry = self.current_action_causal_registry_binding()?;
        let registry_binding = (
            registry.registry_sha256.as_str(),
            registry.catalog_sha256.as_str(),
            registry.source_panel_content_seq,
            registry.source_anchors_cf_last_commit_seq,
            registry.source_anchors_cf_out_of_band_epoch,
            registry.serving_slots.as_slice(),
            registry.serving_slots_sha256.as_str(),
        );
        let expected_registry_binding = (
            expected.causal_registry_sha256.as_str(),
            expected.causal_registry_catalog_sha256.as_str(),
            validation.panel_content_seq,
            validation.anchors_cf_last_commit_seq,
            validation.anchors_cf_out_of_band_epoch,
            expected.causal_registry_serving_slots.as_slice(),
            expected.causal_registry_serving_slots_sha256.as_str(),
        );
        if registry_binding != expected_registry_binding {
            return Err(readiness_publication_stale(
                phase,
                "causal Registry row/content/frontier",
                format!("{expected_registry_binding:?}"),
                format!("{registry_binding:?}"),
            ));
        }

        let guard_profile_sha256 = self
            .current_guard_profile_sha256(ACTION_PANEL_VERSION)?
            .ok_or_else(|| {
                readiness_publication_stale(
                    phase,
                    "action Guard profile",
                    expected.guard_profile_sha256.clone(),
                    "absent".to_owned(),
                )
            })?;
        if guard_profile_sha256 != expected.guard_profile_sha256
            || guard_profile_sha256 != validation.guard_profile_sha256
        {
            return Err(readiness_publication_stale(
                phase,
                "action Guard profile content",
                format!(
                    "readiness={} validation={}",
                    expected.guard_profile_sha256, validation.guard_profile_sha256
                ),
                guard_profile_sha256,
            ));
        }
        Ok(())
    }

    fn ensure_readiness_publication_sources_current(
        &self,
        expected_signals: &SynapseCalyxReadinessSourceSignals,
        expected_evidence: Option<&SynapseCalyxReadinessEvidence>,
        phase: &str,
    ) -> Result<(), SynapseCalyxError> {
        let signals_before = self.action_readiness_source_signals();
        if &signals_before != expected_signals {
            return Err(readiness_publication_stale(
                phase,
                "readiness source CF signals",
                format!("{expected_signals:?}"),
                format!("{signals_before:?}"),
            ));
        }
        self.ensure_admitted_action_evidence_binding_current(expected_evidence, phase)?;
        let signals_after = self.action_readiness_source_signals();
        if &signals_after != expected_signals {
            return Err(readiness_publication_stale(
                phase,
                "readiness source CF signals after binding readback",
                format!("{expected_signals:?}"),
                format!("{signals_after:?}"),
            ));
        }
        Ok(())
    }

    /// Runs the canonical action bits/sufficiency assay, proves the exact
    /// informative-versus-structural-zero partition of the frozen predictor,
    /// then measures and stores all six readiness tiers.
    ///
    /// # Errors
    ///
    /// Refuses any scope/cohort/roster mismatch, partial slot coverage,
    /// estimator refusal that is not an exact constant, source-frontier move,
    /// or readiness publication failure.
    #[expect(
        clippy::too_many_lines,
        reason = "one canonical assay boundary must classify every predictor slot over one source frontier before readiness can authenticate the partition"
    )]
    pub fn measure_action_readiness_from_assay(
        &self,
        full_predictor_panel: &Panel,
        params: &SynapseCalyxAssayParams,
    ) -> Result<SynapseCalyxReadinessSnapshot, SynapseCalyxError> {
        let full_slots = full_predictor_panel
            .slots
            .iter()
            .map(|slot| slot.slot_id.get())
            .collect::<Vec<_>>();
        let required_record_slots =
            std::iter::once(crate::SYNAPSE_CAUSAL_VIEW_REQUIRED_RECORD_SLOT)
                .collect::<std::collections::BTreeSet<_>>();
        let withheld_predictor = ACTION_CAUSAL_PREDICTOR_SLOTS
            .iter()
            .copied()
            .filter(|slot| params.excluded_slots.contains(slot))
            .collect::<Vec<_>>();
        if full_predictor_panel.version != ACTION_PANEL_VERSION
            || full_slots.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS
            || params.panel_version != ACTION_PANEL_VERSION
            || params.corpus_shard != ACTION_DOMAIN
            || params.anchor_kind != ACTION_ANCHOR_KIND
            || params.required_record_slots != required_record_slots
            || !withheld_predictor.is_empty()
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ASSAY_CONTRACT_INVALID",
                format!(
                    "panel_version={} panel_slots={full_slots:?} assay_scope=({}, {}, {}) required_record_slots={:?} withheld_predictor={withheld_predictor:?}",
                    full_predictor_panel.version,
                    params.panel_version,
                    params.corpus_shard,
                    params.anchor_kind,
                    params.required_record_slots,
                ),
                "invoke the canonical action readiness surface with the exact full predictor panel, synapse.action/reward scope, writer-sealed slot-125 cohort, and no withheld predictor causes",
            ));
        }
        let registry = self.current_action_causal_registry_binding()?;
        let expected_excluded_slots = registry
            .declared_slot_ids
            .iter()
            .copied()
            .filter(|slot| !ACTION_CAUSAL_PREDICTOR_SLOTS.contains(slot))
            .collect::<std::collections::BTreeSet<_>>();
        if params.max_records != SYNAPSE_INTELLIGENCE_MAX_RECORDS
            || params.ksg_k != SYNAPSE_KSG_DEFAULT_K
            || params.excluded_slots != expected_excluded_slots
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ASSAY_CONTRACT_INVALID",
                format!(
                    "assay max_records={} ksg_k={} excluded_slots={:?}; required max_records={SYNAPSE_INTELLIGENCE_MAX_RECORDS} ksg_k={SYNAPSE_KSG_DEFAULT_K} exact_nonpredictor_exclusions={expected_excluded_slots:?}",
                    params.max_records, params.ksg_k, params.excluded_slots,
                ),
                "invoke the canonical action readiness surface without tuning its sample, estimator, or feature-selection contract; a different measurement contract requires a new authenticated readiness schema",
            ));
        }
        let anchors_before = self.cf_change_signal(ColumnFamily::Anchors);
        let bits = self.assay_bits(params)?;
        let bits_slots = bits.slots.iter().map(|row| row.slot).collect::<Vec<_>>();
        if bits_slots.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_BITS_ROSTER_INVALID",
                format!(
                    "canonical bits report slots={bits_slots:?}, expected={ACTION_CAUSAL_PREDICTOR_SLOTS:?}"
                ),
                "exclude every non-predictor slot and repair every missing predictor measurement before readiness; the assay roster is never intersected or inferred",
            ));
        }
        let mut informative_slots = Vec::new();
        let mut structural_zero_slots = Vec::new();
        for row in &bits.slots {
            if row.n_samples != bits.anchored_records {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_READINESS_PREDICTOR_COVERAGE_MISMATCH",
                    format!(
                        "predictor slot {} carries {} paired rows but the canonical anchored cohort has {}",
                        row.slot, row.n_samples, bits.anchored_records
                    ),
                    "repair or quarantine incomplete current-panel action rows; every serving cause must cover the exact writer-sealed grounded cohort",
                ));
            }
            if row.state == SynapseCalyxSlotBitsState::Measured {
                informative_slots.push(row.slot);
            } else if row.state == SynapseCalyxSlotBitsState::DegenerateColumn
                && row.distinct_values == Some(1)
            {
                structural_zero_slots.push(row.slot);
            } else {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_READINESS_PREDICTOR_UNMEASURED",
                    format!(
                        "predictor slot {} is neither measured nor exact-constant: state={} distinct_values={:?} samples={} reason={:?}",
                        row.slot,
                        row.state.as_str(),
                        row.distinct_values,
                        row.n_samples,
                        row.unmeasured_reason,
                    ),
                    "collect the named missing/minority-class evidence or repair the estimator contract; only a one-value physical column receives analytical zero-information treatment",
                ));
            }
        }
        if !sufficiency_partition_valid(&informative_slots, &structural_zero_slots) {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SUFFICIENCY_ROSTER_INVALID",
                format!(
                    "informative_slots={informative_slots:?} structural_zero_slots={structural_zero_slots:?} full={ACTION_CAUSAL_PREDICTOR_SLOTS:?}"
                ),
                "repair the canonical bits classification; the two disjoint rosters must cover every frozen predictor slot and at least one must be estimator-backed",
            ));
        }
        let mut sufficiency_params = params.clone();
        sufficiency_params
            .excluded_slots
            .extend(structural_zero_slots.iter().copied());
        let sufficiency = self.assay_sufficiency(&sufficiency_params)?;
        let measured_sufficiency_slots = sufficiency
            .measured_slots
            .iter()
            .map(|slot| slot.get())
            .collect::<Vec<_>>();
        if measured_sufficiency_slots != informative_slots {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ASSAY_ROSTER_MISMATCH",
                format!(
                    "bits informative roster={informative_slots:?}, sufficiency measured roster={measured_sufficiency_slots:?}, structural_zero={structural_zero_slots:?}"
                ),
                "repair the estimator/version skew; readiness never intersects mismatched assay rosters or converts an estimator refusal to zero",
            ));
        }
        if !sufficiency.panel_measured {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_PANEL_UNMEASURED",
                format!(
                    "canonical sufficiency estimator produced no Panel measurement over informative_slots={informative_slots:?}; anchored_records={} joint_records={} structural_zero={structural_zero_slots:?}",
                    sufficiency.anchored_records, sufficiency.joint_records,
                ),
                "collect enough diverse grounded outcomes for at least one predictor view and a measured joint panel; an absent Panel Assay row never reuses an older cached result",
            ));
        }
        let anchors_after = self.cf_change_signal(ColumnFamily::Anchors);
        if anchors_after != anchors_before {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ASSAY_SOURCE_MOVED",
                format!(
                    "Anchors source moved across bits+sufficiency measurement: before={anchors_before:?} after={anchors_after:?}"
                ),
                "rerun oracle_readiness on one stable grounded-outcome frontier; mixed-population Lens and Panel rows never authorize readiness",
            ));
        }
        let mut informative_panel = full_predictor_panel.clone();
        informative_panel
            .slots
            .retain(|slot| informative_slots.contains(&slot.slot_id.get()));
        self.publish_action_readiness(&informative_panel, &structural_zero_slots)
    }

    /// Measures all six readiness tiers from physical vault evidence and stores
    /// the snapshot so frequent health reads remain O(1) and mutation-free.
    ///
    /// # Errors
    ///
    /// Returns a structured error when physical evidence cannot be measured,
    /// encoded, persisted, or independently read back.
    #[expect(
        clippy::too_many_lines,
        reason = "all six tiers must share one measured sequence and one atomic persisted readiness snapshot"
    )]
    fn publish_action_readiness(
        &self,
        panel: &Panel,
        structural_zero_slots: &[u16],
    ) -> Result<SynapseCalyxReadinessSnapshot, SynapseCalyxError> {
        let sufficiency_slots = panel
            .slots
            .iter()
            .map(|slot| slot.slot_id.get())
            .collect::<Vec<_>>();
        let roster_valid = panel.version == ACTION_PANEL_VERSION
            && sufficiency_partition_valid(&sufficiency_slots, structural_zero_slots);
        if !roster_valid {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SUFFICIENCY_ROSTER_INVALID",
                format!(
                    "readiness panel_version={} sufficiency_slots={sufficiency_slots:?} structural_zero_slots={structural_zero_slots:?}; expected a disjoint exact partition of predictor slots {ACTION_CAUSAL_PREDICTOR_SLOTS:?} on panel {ACTION_PANEL_VERSION}",
                    panel.version,
                ),
                "run oracle_readiness through the canonical assay path; it binds only estimator-backed nonconstant predictor views and never invents rows for constants",
            ));
        }
        let structural_zero_slots = structural_zero_slots.to_vec();
        let sufficiency_slots_sha256 =
            sufficiency_slots_sha256(&sufficiency_slots, &structural_zero_slots);
        let measured_at_seq = self.latest_seq();
        let source_signals_before = self.action_readiness_source_signals();
        let domain = DomainId::new(ACTION_DOMAIN);
        let consistency =
            calyx_oracle::oracle_self_consistency_read_only(&self.vault, domain.clone());
        let oracle_clean = match consistency {
            Ok(value) => TierResult::new(
                Tier::OracleClean,
                !value.provisional && value.ceiling >= calyx_oracle::ORACLE_CLEAN_THRESHOLD,
                value.ceiling,
                calyx_oracle::ORACLE_CLEAN_THRESHOLD,
                (value.provisional || value.ceiling < calyx_oracle::ORACLE_CLEAN_THRESHOLD)
                    .then(|| "collect non-provisional recurrence and validity evidence".to_owned()),
            ),
            Err(error) => TierResult::new(
                Tier::OracleClean,
                false,
                0.0,
                calyx_oracle::ORACLE_CLEAN_THRESHOLD,
                Some(error.to_string()),
            ),
        };
        let panel_sufficient = calyx_oracle::measure_tier_panel_sufficient(
            &self.vault,
            panel,
            domain.clone(),
            &calyx_core::SystemClock,
        );
        let kernel_exists = match self.domain_kernel_health(
            panel.version,
            ACTION_CONTENT_SLOT,
            Some(ACTION_ANCHOR_KIND),
        ) {
            Ok(health) => TierResult::new(
                Tier::KernelExists,
                health.recall_ratio >= calyx_oracle::KERNEL_RECALL_RATIO
                    && health.grounded_fraction > 0.0,
                health.recall_ratio,
                calyx_oracle::KERNEL_RECALL_RATIO,
                (health.recall_ratio < calyx_oracle::KERNEL_RECALL_RATIO
                    || health.grounded_fraction <= 0.0)
                    .then(|| {
                        "rebuild the action-domain kernel from grounded held-out records".to_owned()
                    }),
            ),
            Err(error) => failed(
                Tier::KernelExists,
                calyx_oracle::KERNEL_RECALL_RATIO,
                &error,
            ),
        };
        let calibrated = match self.guard_calibration_far(panel.version) {
            Ok(far) => TierResult::new(
                Tier::Calibrated,
                far.is_finite() && far <= calyx_oracle::CALIBRATION_BUDGET,
                far,
                calyx_oracle::CALIBRATION_BUDGET,
                (far > calyx_oracle::CALIBRATION_BUDGET)
                    .then(|| "recalibrate the action-panel guard on held-out bad cases".to_owned()),
            ),
            Err(error) => failed(Tier::Calibrated, calyx_oracle::CALIBRATION_BUDGET, &error),
        };
        let (goodhart, mistakes, evidence, evidence_admission) = self.action_validation_tiers()?;
        let report = SuperIntelReport::new(
            domain,
            vec![
                oracle_clean,
                panel_sufficient,
                kernel_exists,
                calibrated,
                goodhart,
                mistakes,
            ],
        );
        let source_signals_after = self.action_readiness_source_signals();
        if source_signals_after != source_signals_before {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SOURCE_FRONTIER_MOVED",
                format!(
                    "readiness source CF signals changed during measurement: before={source_signals_before:?} after={source_signals_after:?}"
                ),
                "rerun oracle_readiness on a stable Assay/Kernel/Guard/Anchors/Recurrence frontier; a mixed snapshot is never published",
            ));
        }
        let content = StoredReadinessContent {
            schema_version: READINESS_SCHEMA_VERSION,
            domain: ACTION_DOMAIN.to_owned(),
            panel_version: ACTION_PANEL_VERSION,
            report,
            measured_at_seq,
            source_signals: source_signals_before,
            sufficiency_slots,
            structural_zero_slots,
            sufficiency_slots_sha256,
            evidence,
            evidence_admission,
        };
        let content_bytes = serde_json::to_vec(&content).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ENCODE_FAILED",
                format!("encode ledger-bound action readiness content: {error}"),
                "inspect the six-tier readiness report schema",
            )
        })?;
        let content_sha256 = readiness_hex(&Sha256::digest(&content_bytes));
        let ledger_payload = serde_json::to_vec(&ReadinessLedgerPayload {
            tag: "synapse-action-readiness-ledger-v1",
            content_sha256: &content_sha256,
            domain: ACTION_DOMAIN,
            panel_version: ACTION_PANEL_VERSION,
            measured_at_seq,
        })
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ENCODE_FAILED",
                format!("encode action readiness ledger payload: {error}"),
                "inspect the immutable readiness ledger payload schema",
            )
        })?;
        let mut committed_ledger_seq = 0_u64;
        let mut committed_ledger_hash = String::new();
        let mut publication_guard_error: Option<SynapseCalyxError> = None;
        let commit_result = self.vault.append_ledger_entry_with_rows(
            EntryKind::Anneal,
            SubjectId::Query(b"synapse.action/readiness/v7".to_vec()),
            ledger_payload,
            ActorId::Service("synapse-action-readiness".to_owned()),
            |ledger_ref| {
                // Aster invokes this closure while holding its process and
                // cross-process durable commit boundary. Re-read the exact
                // source-CF signals plus the admitted validation,
                // Registry, and Guard identities here, after ledger
                // staging and before either the Anneal row or Ledger row
                // can become visible.
                if let Err(error) = self.ensure_readiness_publication_sources_current(
                    &content.source_signals,
                    content.evidence.as_ref(),
                    "at the atomic readiness+Ledger publication boundary",
                ) {
                    let message = error.to_string();
                    publication_guard_error = Some(error);
                    return Err(calyx_core::CalyxError::ledger_group_commit_failed(message));
                }
                committed_ledger_seq = ledger_ref.seq;
                committed_ledger_hash = readiness_hex(&ledger_ref.hash);
                let stored = StoredReadinessSnapshot {
                    content: content.clone(),
                    content_sha256: content_sha256.clone(),
                    ledger_seq: ledger_ref.seq,
                    ledger_hash: committed_ledger_hash.clone(),
                };
                let value = serde_json::to_vec(&stored).map_err(|error| {
                    calyx_core::CalyxError::ledger_group_commit_failed(format!(
                        "encode stored action readiness snapshot: {error}"
                    ))
                })?;
                Ok(vec![(
                    ColumnFamily::AnnealReport,
                    READINESS_KEY.to_vec(),
                    value,
                )])
            },
        );
        if let Some(error) = publication_guard_error {
            if commit_result.is_ok() {
                return Err(SynapseCalyxError::new(
                    "SYNAPSE_CALYX_READINESS_PUBLICATION_GUARD_BYPASSED",
                    "readiness publication reported success after its in-lock source guard refused",
                    "preserve the WAL and repair the atomic readiness+Ledger publication boundary before trusting any readiness row",
                ));
            }
            return Err(error);
        }
        let persisted_at_seq = commit_result.map_err(|error| {
            SynapseCalyxError::from_calyx(
                "atomically persist action readiness and Anneal ledger",
                &error,
            )
        })?;
        if committed_ledger_seq == 0 || committed_ledger_hash.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_COMMIT_MISSING",
                "readiness group commit completed without a materialized ledger reference",
                "preserve the vault and inspect the atomic AnnealReport+Ledger commit closure",
            ));
        }
        let readback = self.read_action_readiness_raw()?.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_READBACK_MISSING",
                "action readiness row disappeared after commit",
                "inspect the Anneal CF and WAL durability before retrying",
            )
        })?;
        if readback.content_sha256 != content_sha256
            || readback.ledger_seq != committed_ledger_seq
            || readback.ledger_hash != committed_ledger_hash
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_READBACK_MISMATCH",
                format!(
                    "readiness readback content={} ledger=({},{}), committed content={} ledger=({},{})",
                    readback.content_sha256,
                    readback.ledger_seq,
                    readback.ledger_hash,
                    content_sha256,
                    committed_ledger_seq,
                    committed_ledger_hash,
                ),
                "preserve the vault and inspect the physical AnnealReport row plus Ledger entry",
            ));
        }
        self.ensure_readiness_publication_sources_current(
            &readback.source_signals,
            readback.evidence.as_ref(),
            "after readiness physical readback",
        )?;
        Ok(SynapseCalyxReadinessSnapshot {
            schema_version: readback.schema_version,
            domain: readback.domain,
            panel_version: readback.panel_version,
            report: readback.report,
            measured_at_seq,
            persisted_at_seq: Some(persisted_at_seq.seq),
            row_revision_sha256: readback.row_revision_sha256,
            content_sha256: readback.content_sha256,
            ledger_seq: readback.ledger_seq,
            ledger_hash: readback.ledger_hash,
            source_signals: readback.source_signals,
            sufficiency_slots: readback.sufficiency_slots,
            structural_zero_slots: readback.structural_zero_slots,
            sufficiency_slots_sha256: readback.sufficiency_slots_sha256,
            evidence: readback.evidence,
            evidence_admission: readback.evidence_admission,
        })
    }

    /// Admits (or refuses) the persisted held-out report, then derives the two
    /// autonomy tiers from it. Never measures Goodhart or mistake replay itself
    /// — that is `validate_action_readiness`'s job, and readiness deliberately
    /// cannot manufacture its own passing evidence.
    fn action_validation_tiers(
        &self,
    ) -> Result<
        (
            TierResult,
            TierResult,
            Option<SynapseCalyxReadinessEvidence>,
            Vec<SynapseCalyxReadinessPredicate>,
        ),
        SynapseCalyxError,
    > {
        let mut log = Vec::new();
        let mut provenance = None;
        match self.admit_action_evidence(&mut log, &mut provenance) {
            Ok(evidence) => {
                let (goodhart, mistakes) = derive_action_tiers(&evidence, &mut log)?;
                Ok((goodhart, mistakes, provenance, log))
            }
            Err(refusal) => {
                let (goodhart, mistakes) = refused_action_tiers(&refusal);
                Ok((goodhart, mistakes, provenance, log))
            }
        }
    }

    /// The conjunctive admission gate. Predicates are ordered cheapest-and-most
    /// fundamental first, so the reported failure is the root one rather than a
    /// downstream symptom of it.
    #[expect(
        clippy::too_many_lines,
        reason = "the conjunctive admission log must retain its cheapest-first predicate order and exact evidence provenance"
    )]
    fn admit_action_evidence(
        &self,
        log: &mut Vec<SynapseCalyxReadinessPredicate>,
        provenance: &mut Option<SynapseCalyxReadinessEvidence>,
    ) -> Result<SynapseCalyxActionValidationEvidence, AdmissionRefusal> {
        let source = evidence_source();

        // 1. The evidence row physically exists.
        // 2. It decodes and its self-hash still matches (integrity).
        let (evidence, row_revision_sha256) = match self.read_action_validation_revisioned() {
            Ok(Some(row)) => row,
            Ok(None) => {
                return Err(refuse(
                    log,
                    "evidence_row_present",
                    source,
                    "SYNAPSE_CALYX_READINESS_EVIDENCE_ABSENT",
                    format!("no row at {}", evidence_source()),
                    "one held-out action validation row",
                    format!(
                        "run storage operation=intelligence sub_operation=oracle_validate panel_version={ACTION_PANEL_VERSION} to persist a chronological held-out action report at {}",
                        evidence_source()
                    ),
                ));
            }
            Err(error) => {
                return Err(refuse(
                    log,
                    "evidence_integrity",
                    source,
                    error.code,
                    error.message.clone(),
                    "a decodable row whose stored evidence_sha256 matches its own bytes",
                    error.remediation,
                ));
            }
        };
        admit(
            log,
            "evidence_row_present",
            &source,
            format!("row revision {row_revision_sha256}"),
            "one held-out action validation row",
        );
        admit(
            log,
            "evidence_integrity",
            &source,
            "stored evidence_sha256 matches the decoded evidence bytes",
            "self-hash match",
        );
        *provenance = Some(SynapseCalyxReadinessEvidence {
            source_cf: EVIDENCE_CF.to_owned(),
            source_key: evidence_key_name(),
            schema_version: evidence.schema_version,
            row_revision_sha256,
            measured_at_seq: evidence.measured_at_seq,
            panel_content_seq: evidence.panel_content_seq,
            ledger_seq: evidence.ledger_seq,
            ledger_hash: evidence.ledger_hash.clone(),
            ledger_ts_ms: None,
            age_ms: None,
            lease_ms: ACTION_EVIDENCE_LEASE_MS,
            causal_population_contract: evidence.causal_population_contract.clone(),
            source_reward_record_count: evidence.source_reward_record_count,
            action_record_count: evidence.action_record_count,
            action_corpus_sha256: evidence.action_corpus_sha256.clone(),
            excluded_incomplete_causal_records: evidence.excluded_incomplete_causal_records,
            excluded_incomplete_causal_sha256: evidence.excluded_incomplete_causal_sha256.clone(),
            excluded_incomplete_causal_sample: evidence.excluded_incomplete_causal_sample.clone(),
            predictor: evidence.predictor.clone(),
            predictor_slots: evidence.predictor_slots.clone(),
            predictor_sha256: evidence.predictor_sha256.clone(),
            predictor_artifact_blob_id: evidence.predictor_artifact_blob_id.clone(),
            predictor_artifact_blake3: evidence.predictor_artifact_blake3.clone(),
            resource_cause_record_count: evidence.resource_cause_record_count,
            resource_cause_record_sha256: evidence.resource_cause_record_sha256.clone(),
            resource_cause_missing_records: evidence.resource_cause_missing_records,
            resource_cause_missing_sha256: evidence.resource_cause_missing_sha256.clone(),
            causal_registry_sha256: evidence.causal_registry_sha256.clone(),
            causal_registry_catalog_sha256: evidence.causal_registry_catalog_sha256.clone(),
            causal_registry_serving_slots: evidence.causal_registry_serving_slots.clone(),
            causal_registry_serving_slots_sha256: evidence
                .causal_registry_serving_slots_sha256
                .clone(),
            held_out_count: evidence.held_out_count,
            held_out_sha256: evidence.held_out_sha256.clone(),
            guard_profile_sha256: evidence.guard_profile_sha256.clone(),
            goodhart_in_region_frac: evidence.goodhart.in_region_frac,
            goodhart_violations: evidence.goodhart.violations.len(),
            mistake_regression_count: evidence.mistakes.regression_count,
            mistake_regression_evaluated: evidence.regression_evaluated,
        });

        // 3. The evidence is about this domain, this panel, this schema.
        if evidence.schema_version != ACTION_VALIDATION_SCHEMA_VERSION
            || evidence.domain != ACTION_DOMAIN
            || evidence.panel_version != ACTION_PANEL_VERSION
            || evidence.causal_population_contract != ACTION_CAUSAL_POPULATION_CONTRACT
            || evidence.predictor != ACTION_CAUSAL_PREDICTOR
            || evidence.predictor_slots.as_slice() != ACTION_CAUSAL_PREDICTOR_SLOTS
            || evidence.predictor_sha256
                != action_causal_predictor_sha256(&evidence.causal_registry_catalog_sha256)
                    .map_err(|error| AdmissionRefusal {
                        code: error.code.to_owned(),
                        detail: error.message,
                        remediation: error.remediation.to_owned(),
                    })?
            || evidence.causal_registry_serving_slots.as_slice()
                != ACTION_CAUSAL_REGISTRY_SERVING_SLOTS
        {
            return Err(refuse(
                log,
                "evidence_scope",
                source,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_SCOPE_MISMATCH",
                format!(
                    "schema_version={} domain={} panel_version={} causal_population_contract={} predictor={} predictor_slots={:?} predictor_sha256={} causal_registry_serving_slots={:?}",
                    evidence.schema_version,
                    evidence.domain,
                    evidence.panel_version,
                    evidence.causal_population_contract,
                    evidence.predictor,
                    evidence.predictor_slots,
                    evidence.predictor_sha256,
                    evidence.causal_registry_serving_slots,
                ),
                format!(
                    "schema_version={ACTION_VALIDATION_SCHEMA_VERSION} domain={ACTION_DOMAIN} panel_version={ACTION_PANEL_VERSION} causal_population_contract={ACTION_CAUSAL_POPULATION_CONTRACT} predictor={ACTION_CAUSAL_PREDICTOR} predictor_slots={ACTION_CAUSAL_PREDICTOR_SLOTS:?} predictor_sha256=sha256(canonical predictor contract + registry catalog) causal_registry_serving_slots={ACTION_CAUSAL_REGISTRY_SERVING_SLOTS:?}"
                ),
                "discard the mismatched action validation row and rerun oracle_validate for syn-action-v1",
            ));
        }
        admit(
            log,
            "evidence_scope",
            &source,
            format!(
                "schema_version={} domain={} panel_version={} causal_population_contract={} predictor={} predictor_slots={:?} predictor_sha256={} causal_registry_serving_slots={:?}",
                evidence.schema_version,
                evidence.domain,
                evidence.panel_version,
                evidence.causal_population_contract,
                evidence.predictor,
                evidence.predictor_slots,
                evidence.predictor_sha256,
                evidence.causal_registry_serving_slots,
            ),
            format!(
                "schema_version={ACTION_VALIDATION_SCHEMA_VERSION} domain={ACTION_DOMAIN} panel_version={ACTION_PANEL_VERSION} causal_population_contract={ACTION_CAUSAL_POPULATION_CONTRACT} predictor={ACTION_CAUSAL_PREDICTOR} predictor_slots={ACTION_CAUSAL_PREDICTOR_SLOTS:?} predictor_sha256=sha256(canonical predictor contract + registry catalog) causal_registry_serving_slots={ACTION_CAUSAL_REGISTRY_SERVING_SLOTS:?}"
            ),
        );

        // 4. Every grounded reward record is accounted for exactly once as
        //    causally eligible or explicitly excluded. This makes historical
        //    missingness visible and prevents a writer from manufacturing a
        //    cleaner validation population by silently dropping rows.
        let accounted = evidence
            .action_record_count
            .checked_add(evidence.excluded_incomplete_causal_records);
        if accounted != Some(evidence.source_reward_record_count) {
            return Err(refuse(
                log,
                "evidence_population_accounted",
                &source,
                "SYNAPSE_CALYX_READINESS_CAUSAL_POPULATION_UNACCOUNTED",
                format!(
                    "source_reward_record_count={} eligible={} excluded={} accounted={accounted:?}",
                    evidence.source_reward_record_count,
                    evidence.action_record_count,
                    evidence.excluded_incomplete_causal_records
                ),
                "source_reward_record_count == eligible + excluded",
                "quarantine the incomplete evidence row and rerun oracle_validate; validation must bind every grounded reward identity",
            ));
        }
        admit(
            log,
            "evidence_population_accounted",
            &source,
            format!(
                "source_reward_record_count={} eligible={} excluded={}",
                evidence.source_reward_record_count,
                evidence.action_record_count,
                evidence.excluded_incomplete_causal_records
            ),
            "source_reward_record_count == eligible + excluded",
        );

        // 5. Every causally eligible row is accounted for exactly once as
        //    carrying or missing the collection-only resource-prestate cause.
        //    Missing resource state remains visible evidence; it is neither
        //    imputed nor allowed to shrink the serving predictor population.
        let resource_accounted = evidence
            .resource_cause_record_count
            .checked_add(evidence.resource_cause_missing_records);
        if resource_accounted != Some(evidence.action_record_count) {
            return Err(refuse(
                log,
                "evidence_resource_population_accounted",
                &source,
                "SYNAPSE_CALYX_READINESS_RESOURCE_POPULATION_UNACCOUNTED",
                format!(
                    "eligible={} resource_present={} resource_missing={} accounted={resource_accounted:?}",
                    evidence.action_record_count,
                    evidence.resource_cause_record_count,
                    evidence.resource_cause_missing_records,
                ),
                "eligible == resource_present + resource_missing",
                "quarantine the incomplete evidence row and rerun oracle_validate; collection-only resource coverage must bind every eligible identity without imputation",
            ));
        }
        admit(
            log,
            "evidence_resource_population_accounted",
            &source,
            format!(
                "eligible={} resource_present={} resource_missing={}",
                evidence.action_record_count,
                evidence.resource_cause_record_count,
                evidence.resource_cause_missing_records,
            ),
            "eligible == resource_present + resource_missing",
        );

        // 6. The holdout is actually large enough on every side to mean
        //    anything. Re-asserted at read time so a row cannot claim a pass on
        //    an empty holdout regardless of which binary wrote it.
        let floors = [
            ("held_out_count", evidence.held_out_count),
            (
                "guard_training_successes",
                evidence.guard_training_successes,
            ),
            (
                "guard_held_out_successes",
                evidence.guard_held_out_successes,
            ),
            ("regression_evaluated", evidence.regression_evaluated),
        ];
        if let Some((name, value)) = floors
            .iter()
            .find(|(_, value)| *value < MIN_HELD_OUT_RECORDS)
        {
            return Err(refuse(
                log,
                "evidence_complete",
                source,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_INCOMPLETE",
                format!("{name}={value}"),
                format!("{name}>={MIN_HELD_OUT_RECORDS}"),
                "collect more real terminal action outcomes on both sides of the chronological split and rerun oracle_validate",
            ));
        }
        admit(
            log,
            "evidence_complete",
            &source,
            floors
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join(" "),
            format!("every holdout population >= {MIN_HELD_OUT_RECORDS}"),
        );

        // 7. The evidence is bound to a real, self-verifying Anneal ledger
        //    entry. This is what makes the row attributable rather than merely
        //    present: a hand-written row has no matching chain entry.
        let ledger_ts_ms = self.admit_evidence_ledger(log, &evidence)?;
        if let Some(block) = provenance.as_mut() {
            block.ledger_ts_ms = Some(ledger_ts_ms);
        }

        // 8. The exact grounded-outcome frontier has not moved. The validation
        //    ledger and lowered Blob bind every eligible/excluded/resource
        //    identity and vector; this O(1) signal detects any new, removed, or
        //    replaced anchor without rescanning the panel. New unanchored
        //    intent rows deliberately do not invalidate a trained predictor.
        match self.current_action_corpus_binding() {
            Ok(binding) => {
                let unchanged = binding.anchors_cf_last_commit_seq
                    == evidence.anchors_cf_last_commit_seq
                    && binding.anchors_cf_out_of_band_epoch
                        == evidence.anchors_cf_out_of_band_epoch;
                let live = format!(
                    "anchors_cf_last_commit_seq={} anchors_cf_out_of_band_epoch={}",
                    binding.anchors_cf_last_commit_seq, binding.anchors_cf_out_of_band_epoch,
                );
                let expected = format!(
                    "anchors_cf_last_commit_seq={} anchors_cf_out_of_band_epoch={}",
                    evidence.anchors_cf_last_commit_seq, evidence.anchors_cf_out_of_band_epoch,
                );
                if !unchanged {
                    return Err(refuse(
                        log,
                        "evidence_corpus_fresh",
                        CORPUS_SOURCE,
                        "SYNAPSE_CALYX_READINESS_EVIDENCE_CORPUS_MOVED",
                        live,
                        expected,
                        "the grounded-outcome frontier moved after validation; rerun oracle_validate before measuring readiness",
                    ));
                }
                admit(log, "evidence_corpus_fresh", CORPUS_SOURCE, live, expected);
            }
            Err(error) => {
                return Err(refuse(
                    log,
                    "evidence_corpus_fresh",
                    CORPUS_SOURCE,
                    error.code,
                    error.message.clone(),
                    "a readable O(1) Anchors-CF change signal to compare against the ledger-bound lowered corpus",
                    error.remediation,
                ));
            }
        }

        // 9. The exact Registry catalog and its producing, physically serving
        //    slot set have not moved since validation. Collection-only slot
        //    136 stays outside this set until an immutable powered promotion.
        match self.current_action_causal_registry_binding() {
            Ok(binding) => {
                let unchanged = binding.registry_sha256 == evidence.causal_registry_sha256
                    && binding.catalog_sha256 == evidence.causal_registry_catalog_sha256
                    && binding.source_panel_content_seq == evidence.panel_content_seq
                    && binding.source_anchors_cf_last_commit_seq
                        == evidence.anchors_cf_last_commit_seq
                    && binding.source_anchors_cf_out_of_band_epoch
                        == evidence.anchors_cf_out_of_band_epoch
                    && binding.serving_slots == evidence.causal_registry_serving_slots
                    && binding.serving_slots_sha256
                        == evidence.causal_registry_serving_slots_sha256;
                let live = format!(
                    "registry_sha256={} catalog_sha256={} measurement_panel_content_seq={} anchors_cf_last_commit_seq={} anchors_cf_out_of_band_epoch={} serving_slots={:?} serving_slots_sha256={}",
                    binding.registry_sha256,
                    binding.catalog_sha256,
                    binding.source_panel_content_seq,
                    binding.source_anchors_cf_last_commit_seq,
                    binding.source_anchors_cf_out_of_band_epoch,
                    binding.serving_slots,
                    binding.serving_slots_sha256,
                );
                let expected = format!(
                    "registry_sha256={} catalog_sha256={} validation_panel_content_seq={} anchors_cf_last_commit_seq={} anchors_cf_out_of_band_epoch={} serving_slots={:?} serving_slots_sha256={}",
                    evidence.causal_registry_sha256,
                    evidence.causal_registry_catalog_sha256,
                    evidence.panel_content_seq,
                    evidence.anchors_cf_last_commit_seq,
                    evidence.anchors_cf_out_of_band_epoch,
                    evidence.causal_registry_serving_slots,
                    evidence.causal_registry_serving_slots_sha256,
                );
                if !unchanged {
                    return Err(refuse(
                        log,
                        "evidence_causal_registry_fresh",
                        "Registry/causal-view-registry/v5/synapse.action/reward",
                        "SYNAPSE_CALYX_READINESS_CAUSAL_REGISTRY_MOVED",
                        live,
                        expected,
                        "the catalog, measured evidence, or admitted serving causal-view set changed after validation; rerun oracle_validate before measuring readiness",
                    ));
                }
                admit(
                    log,
                    "evidence_causal_registry_fresh",
                    "Registry/causal-view-registry/v5/synapse.action/reward",
                    live,
                    expected,
                );
            }
            Err(error) => {
                return Err(refuse(
                    log,
                    "evidence_causal_registry_fresh",
                    "Registry/causal-view-registry/v5/synapse.action/reward",
                    error.code,
                    error.message.clone(),
                    "a self-verifying canonical causal-view Registry row with the frozen serving set",
                    error.remediation,
                ));
            }
        }

        // 10. The Goodhart boundary itself has not been recalibrated since the
        //    report was scored. An in-region fraction only means something
        //    relative to the exact Ward profile that produced it.
        self.admit_evidence_guard(log, &evidence)?;

        // 11. The report is still inside its freshness lease.
        let age_ms = self.admit_evidence_lease(log, ledger_ts_ms)?;
        if let Some(block) = provenance.as_mut() {
            block.age_ms = Some(age_ms);
        }

        Ok(evidence)
    }

    /// Predicate 7: the evidence's claimed Anneal ledger entry exists, is an
    /// Anneal entry, hashes to what the evidence recorded, and self-verifies.
    /// Returns the entry's commit timestamp, which is the evidence's authentic
    /// wall-clock birth time.
    fn admit_evidence_ledger(
        &self,
        log: &mut Vec<SynapseCalyxReadinessPredicate>,
        evidence: &SynapseCalyxActionValidationEvidence,
    ) -> Result<u64, AdmissionRefusal> {
        let expected_payload_sha256 =
            action_validation_ledger_payload_sha256(evidence).map_err(|error| {
                refuse(
                    log,
                    "evidence_ledger_bound",
                    LEDGER_SOURCE,
                    error.code,
                    error.message.clone(),
                    "deterministically reconstructable validation-ledger payload",
                    error.remediation,
                )
            })?;
        let expected = format!(
            "present anneal entry seq={} hash={} payload_sha256={} self_verifies=true",
            evidence.ledger_seq, evidence.ledger_hash, expected_payload_sha256
        );
        let entry = match self.read_ledger_entry(evidence.ledger_seq) {
            Ok(entry) => entry,
            Err(error) => {
                return Err(refuse(
                    log,
                    "evidence_ledger_bound",
                    LEDGER_SOURCE,
                    error.code,
                    error.message.clone(),
                    expected,
                    error.remediation,
                ));
            }
        };
        let observed = format!(
            "present={} kind={} hash={} payload_sha256={} self_verifies={}",
            entry.present,
            entry.kind.as_deref().unwrap_or("<none>"),
            entry.entry_hash.as_deref().unwrap_or("<none>"),
            entry.payload_sha256.as_deref().unwrap_or("<none>"),
            entry
                .self_verifies
                .map_or_else(|| "<none>".to_owned(), |value| value.to_string()),
        );
        let bound = entry.present
            && entry.kind.as_deref() == Some("anneal")
            && entry.entry_hash.as_deref() == Some(evidence.ledger_hash.as_str())
            && entry.payload_sha256.as_deref() == Some(expected_payload_sha256.as_str())
            && entry.self_verifies == Some(true);
        let Some(ts_ms) = entry.ts.filter(|_| bound) else {
            return Err(refuse(
                log,
                "evidence_ledger_bound",
                LEDGER_SOURCE,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_LEDGER_UNBOUND",
                observed,
                expected,
                "the action validation row is not attributable to a self-verifying Anneal ledger entry; quarantine the row and rerun oracle_validate",
            ));
        };
        admit(
            log,
            "evidence_ledger_bound",
            LEDGER_SOURCE,
            format!("{observed} ts_ms={ts_ms}"),
            expected,
        );
        Ok(ts_ms)
    }

    /// Predicate 10: the live Ward profile is byte-identical to the one the
    /// held-out Goodhart report was scored against.
    fn admit_evidence_guard(
        &self,
        log: &mut Vec<SynapseCalyxReadinessPredicate>,
        evidence: &SynapseCalyxActionValidationEvidence,
    ) -> Result<(), AdmissionRefusal> {
        let expected = format!("sha256={}", evidence.guard_profile_sha256);
        match self.current_guard_profile_sha256(evidence.panel_version) {
            Ok(Some(current)) if current == evidence.guard_profile_sha256 => {
                admit(
                    log,
                    "evidence_guard_fresh",
                    GUARD_SOURCE,
                    format!("sha256={current}"),
                    expected,
                );
                Ok(())
            }
            Ok(Some(current)) => Err(refuse(
                log,
                "evidence_guard_fresh",
                GUARD_SOURCE,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_GUARD_REBOUND",
                format!("sha256={current}"),
                expected,
                "the action-panel Ward profile was recalibrated after validation; rerun oracle_validate so the Goodhart holdout is scored against the live boundary",
            )),
            Ok(None) => Err(refuse(
                log,
                "evidence_guard_fresh",
                GUARD_SOURCE,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_GUARD_ABSENT",
                "no Ward profile row for the action panel",
                expected,
                "calibrate the action-panel guard from real good and bad cases, then rerun oracle_validate",
            )),
            Err(error) => Err(refuse(
                log,
                "evidence_guard_fresh",
                GUARD_SOURCE,
                error.code,
                error.message.clone(),
                expected,
                error.remediation,
            )),
        }
    }

    /// Predicate 11: the evidence is inside its freshness lease. Returns its age.
    fn admit_evidence_lease(
        &self,
        log: &mut Vec<SynapseCalyxReadinessPredicate>,
        ledger_ts_ms: u64,
    ) -> Result<u64, AdmissionRefusal> {
        let expected = format!("age_ms <= {ACTION_EVIDENCE_LEASE_MS}");
        let now_ms = match self.clock_now_ms() {
            Ok(now) => now,
            Err(error) => {
                return Err(refuse(
                    log,
                    "evidence_lease",
                    LEDGER_SOURCE,
                    error.code,
                    error.message.clone(),
                    expected,
                    error.remediation,
                ));
            }
        };
        let Some(age_ms) = now_ms.checked_sub(ledger_ts_ms) else {
            return Err(refuse(
                log,
                "evidence_lease",
                LEDGER_SOURCE,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_CLOCK_SKEW",
                format!("evidence ledger ts_ms={ledger_ts_ms} is ahead of vault now_ms={now_ms}"),
                expected,
                "the evidence is stamped in the future relative to this vault's clock; reconcile the vault clock and rerun oracle_validate",
            ));
        };
        if age_ms > ACTION_EVIDENCE_LEASE_MS {
            return Err(refuse(
                log,
                "evidence_lease",
                LEDGER_SOURCE,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_STALE",
                format!("age_ms={age_ms} (ledger ts_ms={ledger_ts_ms}, now_ms={now_ms})"),
                expected,
                "the held-out action report has outlived its freshness lease; rerun oracle_validate to re-earn autonomy on current evidence",
            ));
        }
        admit(
            log,
            "evidence_lease",
            LEDGER_SOURCE,
            format!("age_ms={age_ms} (ledger ts_ms={ledger_ts_ms}, now_ms={now_ms})"),
            expected,
        );
        Ok(age_ms)
    }

    /// Reads the last persisted readiness snapshot without recomputation and
    /// proves that every mutable authority it names is still current.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the physical row cannot be read, decoded,
    /// matched to the current readiness schema/domain/panel, or matched to the
    /// exact source signals, validation revision, Registry, and Guard it binds.
    pub fn read_action_readiness(
        &self,
    ) -> Result<Option<SynapseCalyxReadinessSnapshot>, SynapseCalyxError> {
        let snapshot = self.read_action_readiness_raw()?;
        if let Some(snapshot) = snapshot.as_ref() {
            self.ensure_readiness_publication_sources_current(
                &snapshot.source_signals,
                snapshot.evidence.as_ref(),
                "while serving the current readiness row",
            )?;
        }
        Ok(snapshot)
    }

    /// Reads and authenticates the physical historical row without claiming
    /// that its mutable sources are still current. This is intentionally
    /// private: operational callers must use [`Self::read_action_readiness`].
    #[expect(
        clippy::too_many_lines,
        reason = "decode, content verification, Ledger binding, and public projection form one authenticated point-read"
    )]
    fn read_action_readiness_raw(
        &self,
    ) -> Result<Option<SynapseCalyxReadinessSnapshot>, SynapseCalyxError> {
        let Some(row) =
            self.read_cf_latest_revisioned(ColumnFamily::AnnealReport, READINESS_KEY)?
        else {
            return Ok(None);
        };
        let stored: StoredReadinessSnapshot =
            serde_json::from_slice(&row.value).map_err(|error| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_READINESS_DECODE_FAILED",
                    format!("decode persisted action readiness report: {error}"),
                    "quarantine the corrupt Anneal row and remeasure readiness",
                )
            })?;
        let content_bytes = serde_json::to_vec(&stored.content).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ENCODE_FAILED",
                format!("re-encode persisted readiness content: {error}"),
                "preserve the row and inspect deterministic readiness serialization",
            )
        })?;
        let content_sha256 = readiness_hex(&Sha256::digest(&content_bytes));
        if content_sha256 != stored.content_sha256 {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_INTEGRITY_FAILED",
                format!(
                    "readiness content sha256={content_sha256} differs from stored {}",
                    stored.content_sha256
                ),
                "quarantine the tampered AnnealReport row and remeasure readiness",
            ));
        }
        let payload = serde_json::to_vec(&ReadinessLedgerPayload {
            tag: "synapse-action-readiness-ledger-v1",
            content_sha256: &stored.content_sha256,
            domain: &stored.content.domain,
            panel_version: stored.content.panel_version,
            measured_at_seq: stored.content.measured_at_seq,
        })
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ENCODE_FAILED",
                format!("reconstruct readiness ledger payload: {error}"),
                "preserve the row and inspect deterministic ledger serialization",
            )
        })?;
        let expected_payload_sha256 = readiness_hex(&Sha256::digest(payload));
        let ledger = self.read_ledger_entry(stored.ledger_seq)?;
        if !ledger.present
            || ledger.kind.as_deref() != Some(EntryKind::Anneal.as_str())
            || ledger.entry_hash.as_deref() != Some(stored.ledger_hash.as_str())
            || ledger.payload_sha256.as_deref() != Some(expected_payload_sha256.as_str())
            || ledger.self_verifies != Some(true)
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_LEDGER_UNBOUND",
                format!(
                    "readiness ledger seq={} hash={} readback present={} kind={:?} hash={:?} payload={:?} expected_payload={} self_verifies={:?}",
                    stored.ledger_seq,
                    stored.ledger_hash,
                    ledger.present,
                    ledger.kind,
                    ledger.entry_hash,
                    ledger.payload_sha256,
                    expected_payload_sha256,
                    ledger.self_verifies,
                ),
                "quarantine the unbound readiness row and remeasure through the atomic readiness+Ledger writer",
            ));
        }
        if stored.content.schema_version != READINESS_SCHEMA_VERSION {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SCHEMA_MISMATCH",
                format!(
                    "persisted action readiness row is schema_version {}, this build reads {READINESS_SCHEMA_VERSION}",
                    stored.content.schema_version
                ),
                "remeasure readiness so the persisted row carries this build's evidence provenance",
            ));
        }
        if stored.content.domain != ACTION_DOMAIN
            || stored.content.panel_version != ACTION_PANEL_VERSION
        {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SCOPE_MISMATCH",
                format!(
                    "persisted action readiness row has domain={} panel_version={}; this build requires domain={ACTION_DOMAIN} panel_version={ACTION_PANEL_VERSION}",
                    stored.content.domain, stored.content.panel_version
                ),
                "preserve the mismatched row as historical evidence and remeasure readiness for the current action panel; reads never reinterpret a different frozen panel generation",
            ));
        }
        let sufficiency_roster_valid = sufficiency_partition_valid(
            &stored.content.sufficiency_slots,
            &stored.content.structural_zero_slots,
        ) && stored.content.sufficiency_slots_sha256
            == sufficiency_slots_sha256(
                &stored.content.sufficiency_slots,
                &stored.content.structural_zero_slots,
            );
        if !sufficiency_roster_valid {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SUFFICIENCY_ROSTER_INVALID",
                format!(
                    "persisted readiness sufficiency_slots={:?} structural_zero_slots={:?} sha256={}",
                    stored.content.sufficiency_slots,
                    stored.content.structural_zero_slots,
                    stored.content.sufficiency_slots_sha256,
                ),
                "quarantine the malformed readiness row and remeasure through the canonical estimator-backed action assay",
            ));
        }
        let content = stored.content;
        Ok(Some(SynapseCalyxReadinessSnapshot {
            schema_version: content.schema_version,
            domain: content.domain,
            panel_version: content.panel_version,
            report: content.report,
            measured_at_seq: content.measured_at_seq,
            persisted_at_seq: None,
            row_revision_sha256: {
                use std::fmt::Write as _;

                row.revision_sha256.iter().fold(
                    String::with_capacity(row.revision_sha256.len() * 2),
                    |mut out, byte| {
                        let _ = write!(out, "{byte:02x}");
                        out
                    },
                )
            },
            content_sha256: stored.content_sha256,
            ledger_seq: stored.ledger_seq,
            ledger_hash: stored.ledger_hash,
            source_signals: content.source_signals,
            sufficiency_slots: content.sufficiency_slots,
            structural_zero_slots: content.structural_zero_slots,
            sufficiency_slots_sha256: content.sufficiency_slots_sha256,
            evidence: content.evidence,
            evidence_admission: content.evidence_admission,
        }))
    }
}

/// Derives the two autonomy tiers from admitted evidence, logging each as its
/// own predicate so the tier value and its source number stay side by side.
#[expect(
    clippy::too_many_lines,
    reason = "both autonomy tiers and their predicate evidence must be derived from one admitted report without splitting provenance"
)]
fn derive_action_tiers(
    evidence: &SynapseCalyxActionValidationEvidence,
    log: &mut Vec<SynapseCalyxReadinessPredicate>,
) -> Result<(TierResult, TierResult), SynapseCalyxError> {
    let source = evidence_source();

    let in_region_frac = evidence
        .goodhart
        .in_region_frac
        .ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_EVIDENCE_METRIC_MISSING",
                "persisted Goodhart evidence has no in_region_frac measurement",
                "preserve the evidence row and rerun held-out action validation to produce the required metric",
            )
        })?
        .to_f32()
        .ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_EVIDENCE_NUMERIC_OUT_OF_RANGE",
                "persisted Goodhart in_region_frac cannot be represented as f32",
                "preserve the evidence row and rerun held-out validation with finite bounded metrics",
            )
        })?;
    let goodhart_passed = evidence.goodhart.passed
        && evidence.goodhart.violations.is_empty()
        && in_region_frac.is_finite()
        && in_region_frac >= calyx_oracle::GOODHART_THRESHOLD;
    let goodhart_fix = "held-out successful actions crossed the calibrated Ward boundary; inspect the persisted Goodhart violations in the action validation row and recalibrate from real outcomes";
    let goodhart_measured = format!(
        "goodhart.passed={} violations={} in_region_frac={in_region_frac}",
        evidence.goodhart.passed,
        evidence.goodhart.violations.len()
    );
    let goodhart_expected = format!(
        "passed=true violations=0 in_region_frac>={}",
        calyx_oracle::GOODHART_THRESHOLD
    );
    if goodhart_passed {
        admit(
            log,
            "goodhart_defended",
            &source,
            goodhart_measured,
            goodhart_expected,
        );
    } else {
        refuse(
            log,
            "goodhart_defended",
            &source,
            "SYNAPSE_CALYX_READINESS_GOODHART_UNDEFENDED",
            goodhart_measured,
            goodhart_expected,
            goodhart_fix,
        );
    }
    let goodhart = TierResult::new(
        Tier::GoodhartDefended,
        goodhart_passed,
        in_region_frac,
        calyx_oracle::GOODHART_THRESHOLD,
        (!goodhart_passed).then(|| goodhart_fix.to_owned()),
    );

    // A report whose regression_count disagrees with its own result rows is
    // not a passing report — it is an inconsistent one, and it refuses here
    // rather than being read as zero regressions.
    let mistakes_consistent = calyx_anneal::regression_rate(&evidence.mistakes).is_ok();
    let mistakes_passed =
        mistakes_consistent && evidence.mistakes.passed && evidence.mistakes.regression_count == 0;
    let mistakes_fix = if mistakes_consistent {
        "one or more chronological action mistakes still recur under current evidence; improve the action predictor and rerun oracle_validate"
    } else {
        "the persisted mistake-replay report's regression_count disagrees with its own result rows; quarantine the row and rerun oracle_validate"
    };
    let mistakes_measured = format!(
        "consistent={mistakes_consistent} passed={} regression_count={} evaluated={}",
        evidence.mistakes.passed, evidence.mistakes.regression_count, evidence.regression_evaluated
    );
    let mistakes_expected = "consistent=true passed=true regression_count=0";
    if mistakes_passed {
        admit(
            log,
            "mistake_closed",
            &source,
            mistakes_measured,
            mistakes_expected,
        );
    } else {
        refuse(
            log,
            "mistake_closed",
            &source,
            if mistakes_consistent {
                "SYNAPSE_CALYX_READINESS_MISTAKES_RECUR"
            } else {
                "SYNAPSE_CALYX_READINESS_MISTAKE_REPORT_INCONSISTENT"
            },
            mistakes_measured,
            mistakes_expected,
            mistakes_fix,
        );
    }
    let regression_count = evidence.mistakes.regression_count.to_f32().ok_or_else(|| {
        SynapseCalyxError::new(
            "SYNAPSE_CALYX_READINESS_EVIDENCE_NUMERIC_OUT_OF_RANGE",
            format!(
                "persisted regression count {} cannot be represented as f32",
                evidence.mistakes.regression_count
            ),
            "preserve the evidence row and rerun held-out validation with a bounded replay corpus",
        )
    })?;
    let mistakes = TierResult::new(
        Tier::MistakeClosed,
        mistakes_passed,
        regression_count,
        0.0,
        (!mistakes_passed).then(|| mistakes_fix.to_owned()),
    );

    Ok((goodhart, mistakes))
}

/// Both autonomy tiers when the evidence itself was refused. They carry the
/// exact failing predicate code and its remediation rather than a bare `false`.
fn refused_action_tiers(refusal: &AdmissionRefusal) -> (TierResult, TierResult) {
    let fix = refusal.tier_fix();
    (
        TierResult::new(
            Tier::GoodhartDefended,
            false,
            0.0,
            calyx_oracle::GOODHART_THRESHOLD,
            Some(fix.clone()),
        ),
        // 1.0, not NaN: the persisted row must survive a JSON round-trip, and
        // "at least one undemonstrated regression" is the honest reading of
        // evidence that was never admitted.
        TierResult::new(Tier::MistakeClosed, false, 1.0, 0.0, Some(fix)),
    )
}

fn failed(tier: Tier, threshold: f32, error: &SynapseCalyxError) -> TierResult {
    TierResult::new(tier, false, 0.0, threshold, Some(error.to_string()))
}
