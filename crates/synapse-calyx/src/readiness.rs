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
use calyx_oracle::{DomainId, SuperIntelReport, Tier, TierResult};
use serde::{Deserialize, Serialize};

use crate::action_validation::{
    ACTION_PANEL_VERSION, ACTION_VALIDATION_KEY, ACTION_VALIDATION_SCHEMA_VERSION,
    MIN_HELD_OUT_RECORDS,
};
use crate::{
    SynapseCalyxActionValidationEvidence, SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault,
};

const ACTION_DOMAIN: &str = "synapse.action";
const ACTION_CONTENT_SLOT: u16 = 50;
const ACTION_ANCHOR_KIND: &str = "reward";
/// `v2` carries the evidence provenance block and the admission log. A `v1`
/// row cannot say which report it measured, so it is not read: readiness
/// reports an absent snapshot and every autonomy consumer fails closed.
const READINESS_KEY: &[u8] = b"oracle-readiness/v2/synapse.action";
const READINESS_SCHEMA_VERSION: u32 = 2;
const EVIDENCE_CF: &str = "AnnealReport";
const LEDGER_SOURCE: &str = "Ledger/anneal";
const GUARD_SOURCE: &str = "Guard/profile\\0panel\\0<panel_version>";
const CORPUS_SOURCE: &str = "Base/panel=2020001,oracle.domain=synapse.action";

/// Freshness lease on held-out action evidence, in milliseconds (7 days).
///
/// The corpus binding below is an *exact* version precondition: the moment a
/// single new terminal action outcome lands, the evidence stops matching and
/// readiness refuses. So this lease only governs the quiescent case — a vault
/// that has recorded no new action outcome at all for a week. That silence is
/// not evidence that autonomy is still safe, so a passing report is not
/// allowed to authorize autonomy indefinitely on the strength of its age.
const EVIDENCE_LEASE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// One named admission predicate and everything needed to audit its verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    pub ledger_seq: u64,
    pub ledger_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_ts_ms: Option<u64>,
    /// Age of the evidence at measurement time, from its own ledger entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    pub lease_ms: u64,
    pub action_record_count: usize,
    pub action_corpus_sha256: String,
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
    pub report: SuperIntelReport,
    pub measured_at_seq: u64,
    pub persisted_at_seq: Option<u64>,
    pub row_revision_sha256: String,
    /// The report the autonomy tiers were admitted from; `None` when no
    /// admissible evidence row could be read at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SynapseCalyxReadinessEvidence>,
    /// Every admission predicate in evaluation order, passing and failing.
    pub evidence_admission: Vec<SynapseCalyxReadinessPredicate>,
}

#[derive(Serialize, Deserialize)]
struct StoredReadinessSnapshot {
    schema_version: u32,
    report: SuperIntelReport,
    measured_at_seq: u64,
    #[serde(default)]
    evidence: Option<SynapseCalyxReadinessEvidence>,
    #[serde(default)]
    evidence_admission: Vec<SynapseCalyxReadinessPredicate>,
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

impl SynapseCalyxVault {
    /// Measures all six readiness tiers from physical vault evidence and stores
    /// the snapshot so frequent health reads remain O(1) and mutation-free.
    pub fn measure_action_readiness(
        &self,
        panel: &Panel,
    ) -> Result<SynapseCalyxReadinessSnapshot, SynapseCalyxError> {
        let measured_at_seq = self.latest_seq();
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
                Some(error.remediation().to_owned()),
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
        let (goodhart, mistakes, evidence, evidence_admission) = self.action_validation_tiers();
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
        let encoded = serde_json::to_vec(&StoredReadinessSnapshot {
            schema_version: READINESS_SCHEMA_VERSION,
            report,
            measured_at_seq,
            evidence,
            evidence_admission,
        })
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_ENCODE_FAILED",
                format!("encode action readiness report: {error}"),
                "inspect the six-tier readiness report schema",
            )
        })?;
        let persisted_at_seq = self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::AnnealReport,
            key: READINESS_KEY.to_vec(),
            value: encoded,
        }])?;
        let readback = self.read_action_readiness()?.ok_or_else(|| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_READBACK_MISSING",
                "action readiness row disappeared after commit",
                "inspect the Anneal CF and WAL durability before retrying",
            )
        })?;
        Ok(SynapseCalyxReadinessSnapshot {
            schema_version: readback.schema_version,
            report: readback.report,
            measured_at_seq,
            persisted_at_seq: Some(persisted_at_seq),
            row_revision_sha256: readback.row_revision_sha256,
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
    ) -> (
        TierResult,
        TierResult,
        Option<SynapseCalyxReadinessEvidence>,
        Vec<SynapseCalyxReadinessPredicate>,
    ) {
        let mut log = Vec::new();
        let mut provenance = None;
        match self.admit_action_evidence(&mut log, &mut provenance) {
            Ok(evidence) => {
                let (goodhart, mistakes) = derive_action_tiers(&evidence, &mut log);
                (goodhart, mistakes, provenance, log)
            }
            Err(refusal) => {
                let (goodhart, mistakes) = refused_action_tiers(&refusal);
                (goodhart, mistakes, provenance, log)
            }
        }
    }

    /// The conjunctive admission gate. Predicates are ordered cheapest-and-most
    /// fundamental first, so the reported failure is the root one rather than a
    /// downstream symptom of it.
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
            ledger_seq: evidence.ledger_seq,
            ledger_hash: evidence.ledger_hash.clone(),
            ledger_ts_ms: None,
            age_ms: None,
            lease_ms: EVIDENCE_LEASE_MS,
            action_record_count: evidence.action_record_count,
            action_corpus_sha256: evidence.action_corpus_sha256.clone(),
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
        {
            return Err(refuse(
                log,
                "evidence_scope",
                source,
                "SYNAPSE_CALYX_READINESS_EVIDENCE_SCOPE_MISMATCH",
                format!(
                    "schema_version={} domain={} panel_version={}",
                    evidence.schema_version, evidence.domain, evidence.panel_version
                ),
                format!(
                    "schema_version={ACTION_VALIDATION_SCHEMA_VERSION} domain={ACTION_DOMAIN} panel_version={ACTION_PANEL_VERSION}"
                ),
                "discard the mismatched action validation row and rerun oracle_validate for syn-action-v1",
            ));
        }
        admit(
            log,
            "evidence_scope",
            &source,
            format!(
                "schema_version={} domain={} panel_version={}",
                evidence.schema_version, evidence.domain, evidence.panel_version
            ),
            format!(
                "schema_version={ACTION_VALIDATION_SCHEMA_VERSION} domain={ACTION_DOMAIN} panel_version={ACTION_PANEL_VERSION}"
            ),
        );

        // 4. The holdout is actually large enough on every side to mean
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

        // 5. The evidence is bound to a real, self-verifying Anneal ledger
        //    entry. This is what makes the row attributable rather than merely
        //    present: a hand-written row has no matching chain entry.
        let ledger_ts_ms = self.admit_evidence_ledger(log, &evidence)?;
        if let Some(block) = provenance.as_mut() {
            block.ledger_ts_ms = Some(ledger_ts_ms);
        }

        // 6. The action corpus has not moved since the report was scored: an
        //    exact version precondition, not a tolerance.
        match self.current_action_corpus_binding() {
            Ok((count, hash)) => {
                if count != evidence.action_record_count || hash != evidence.action_corpus_sha256 {
                    return Err(refuse(
                        log,
                        "evidence_corpus_fresh",
                        CORPUS_SOURCE,
                        "SYNAPSE_CALYX_READINESS_EVIDENCE_CORPUS_MOVED",
                        format!("live corpus count={count} sha256={hash}"),
                        format!(
                            "count={} sha256={}",
                            evidence.action_record_count, evidence.action_corpus_sha256
                        ),
                        "action outcomes changed after validation; rerun oracle_validate before measuring readiness",
                    ));
                }
                admit(
                    log,
                    "evidence_corpus_fresh",
                    CORPUS_SOURCE,
                    format!("count={count} sha256={hash}"),
                    "byte-identical to the corpus the held-out report was scored on",
                );
            }
            Err(error) => {
                return Err(refuse(
                    log,
                    "evidence_corpus_fresh",
                    CORPUS_SOURCE,
                    error.code,
                    error.message.clone(),
                    "a readable live action corpus to compare against the evidence binding",
                    error.remediation,
                ));
            }
        }

        // 7. The Goodhart boundary itself has not been recalibrated since the
        //    report was scored. An in-region fraction only means something
        //    relative to the exact Ward profile that produced it.
        self.admit_evidence_guard(log, &evidence)?;

        // 8. The report is still inside its freshness lease.
        let age_ms = self.admit_evidence_lease(log, ledger_ts_ms)?;
        if let Some(block) = provenance.as_mut() {
            block.age_ms = Some(age_ms);
        }

        Ok(evidence)
    }

    /// Predicate 5: the evidence's claimed Anneal ledger entry exists, is an
    /// Anneal entry, hashes to what the evidence recorded, and self-verifies.
    /// Returns the entry's commit timestamp, which is the evidence's authentic
    /// wall-clock birth time.
    fn admit_evidence_ledger(
        &self,
        log: &mut Vec<SynapseCalyxReadinessPredicate>,
        evidence: &SynapseCalyxActionValidationEvidence,
    ) -> Result<u64, AdmissionRefusal> {
        let expected = format!(
            "present anneal entry seq={} hash={} self_verifies=true",
            evidence.ledger_seq, evidence.ledger_hash
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
            "present={} kind={} hash={} self_verifies={}",
            entry.present,
            entry.kind.as_deref().unwrap_or("<none>"),
            entry.entry_hash.as_deref().unwrap_or("<none>"),
            entry
                .self_verifies
                .map_or_else(|| "<none>".to_owned(), |value| value.to_string()),
        );
        let bound = entry.present
            && entry.kind.as_deref() == Some("anneal")
            && entry.entry_hash.as_deref() == Some(evidence.ledger_hash.as_str())
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

    /// Predicate 7: the live Ward profile is byte-identical to the one the
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

    /// Predicate 8: the evidence is inside its freshness lease. Returns its age.
    fn admit_evidence_lease(
        &self,
        log: &mut Vec<SynapseCalyxReadinessPredicate>,
        ledger_ts_ms: u64,
    ) -> Result<u64, AdmissionRefusal> {
        let expected = format!("age_ms <= {EVIDENCE_LEASE_MS}");
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
        if age_ms > EVIDENCE_LEASE_MS {
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

    /// Reads the last persisted readiness snapshot without recomputation.
    pub fn read_action_readiness(
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
        if stored.schema_version != READINESS_SCHEMA_VERSION {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_READINESS_SCHEMA_MISMATCH",
                format!(
                    "persisted action readiness row is schema_version {}, this build reads {READINESS_SCHEMA_VERSION}",
                    stored.schema_version
                ),
                "remeasure readiness so the persisted row carries this build's evidence provenance",
            ));
        }
        Ok(Some(SynapseCalyxReadinessSnapshot {
            schema_version: stored.schema_version,
            report: stored.report,
            measured_at_seq: stored.measured_at_seq,
            persisted_at_seq: None,
            row_revision_sha256: row
                .revision_sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            evidence: stored.evidence,
            evidence_admission: stored.evidence_admission,
        }))
    }
}

/// Derives the two autonomy tiers from admitted evidence, logging each as its
/// own predicate so the tier value and its source number stay side by side.
fn derive_action_tiers(
    evidence: &SynapseCalyxActionValidationEvidence,
    log: &mut Vec<SynapseCalyxReadinessPredicate>,
) -> (TierResult, TierResult) {
    let source = evidence_source();

    let in_region_frac = evidence.goodhart.in_region_frac.unwrap_or(0.0) as f32;
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
    let mistakes = TierResult::new(
        Tier::MistakeClosed,
        mistakes_passed,
        evidence.mistakes.regression_count as f32,
        0.0,
        (!mistakes_passed).then(|| mistakes_fix.to_owned()),
    );

    (goodhart, mistakes)
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
    TierResult::new(
        tier,
        false,
        0.0,
        threshold,
        Some(error.remediation.to_owned()),
    )
}
