//! Persisted, read-only health snapshot for the action Oracle readiness gate.

use calyx_aster::cf::ColumnFamily;
use calyx_core::Panel;
use calyx_oracle::{DomainId, SuperIntelReport, Tier, TierResult};
use serde::{Deserialize, Serialize};

use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

const ACTION_DOMAIN: &str = "synapse.action";
const ACTION_CONTENT_SLOT: u16 = 50;
const ACTION_ANCHOR_KIND: &str = "reward";
const READINESS_KEY: &[u8] = b"oracle-readiness/v1/synapse.action";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxReadinessSnapshot {
    pub report: SuperIntelReport,
    pub measured_at_seq: u64,
    pub persisted_at_seq: Option<u64>,
    pub row_revision_sha256: String,
}

#[derive(Serialize, Deserialize)]
struct StoredReadinessSnapshot {
    report: SuperIntelReport,
    measured_at_seq: u64,
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
        let (goodhart, mistakes) = self.action_validation_tiers();
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
            report,
            measured_at_seq,
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
            report: readback.report,
            measured_at_seq,
            persisted_at_seq: Some(persisted_at_seq),
            row_revision_sha256: readback.row_revision_sha256,
        })
    }

    fn action_validation_tiers(&self) -> (TierResult, TierResult) {
        let evidence = match self.read_action_validation() {
            Ok(Some(evidence)) => evidence,
            Ok(None) => {
                return validation_failed(
                    "run storage intelligence oracle_validate to persist a chronological action-domain holdout",
                );
            }
            Err(error) => return validation_failed(error.remediation),
        };
        if evidence.domain != ACTION_DOMAIN || evidence.panel_version != 2_020_001 {
            return validation_failed(
                "discard the mismatched action validation row and rerun oracle_validate for syn-action-v1",
            );
        }
        let (current_count, current_hash) = match self.current_action_corpus_binding() {
            Ok(binding) => binding,
            Err(error) => return validation_failed(error.remediation),
        };
        if current_count != evidence.action_record_count
            || current_hash != evidence.action_corpus_sha256
        {
            return validation_failed(
                "action outcomes changed after validation; rerun oracle_validate before measuring readiness",
            );
        }
        let pass_rate = evidence.goodhart.in_region_frac.unwrap_or(0.0) as f32;
        let goodhart_passed = evidence.goodhart.passed
            && pass_rate.is_finite()
            && pass_rate >= calyx_oracle::GOODHART_THRESHOLD;
        let goodhart = TierResult::new(
            Tier::GoodhartDefended,
            goodhart_passed,
            pass_rate,
            calyx_oracle::GOODHART_THRESHOLD,
            (!goodhart_passed).then(|| {
                "held-out successful actions crossed the calibrated Ward boundary; inspect the persisted Goodhart violations and recalibrate from real outcomes".to_owned()
            }),
        );
        let mistakes_valid = calyx_anneal::regression_rate(&evidence.mistakes).is_ok();
        let mistakes_passed =
            mistakes_valid && evidence.mistakes.passed && evidence.mistakes.regression_count == 0;
        let mistakes = TierResult::new(
            Tier::MistakeClosed,
            mistakes_passed,
            evidence.mistakes.regression_count as f32,
            0.0,
            (!mistakes_passed).then(|| {
                "one or more chronological action mistakes still recur under current evidence; improve the action predictor and rerun oracle_validate".to_owned()
            }),
        );
        (goodhart, mistakes)
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
        Ok(Some(SynapseCalyxReadinessSnapshot {
            report: stored.report,
            measured_at_seq: stored.measured_at_seq,
            persisted_at_seq: None,
            row_revision_sha256: row
                .revision_sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }))
    }
}

fn validation_failed(remediation: impl Into<String>) -> (TierResult, TierResult) {
    let remediation = remediation.into();
    (
        TierResult::new(
            Tier::GoodhartDefended,
            false,
            0.0,
            calyx_oracle::GOODHART_THRESHOLD,
            Some(remediation.clone()),
        ),
        TierResult::new(Tier::MistakeClosed, false, 1.0, 0.0, Some(remediation)),
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
