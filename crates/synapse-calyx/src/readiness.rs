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
        // #1681 owns these two persisted measurement sources. Absence is an
        // explicit failed tier, which is the fail-closed autonomy behavior.
        let goodhart = TierResult::new(
            Tier::GoodhartDefended,
            false,
            0.0,
            calyx_oracle::GOODHART_THRESHOLD,
            Some("run and persist the Anneal held-out Goodhart defense report".to_owned()),
        );
        let mistakes = TierResult::new(
            Tier::MistakeClosed,
            false,
            1.0,
            0.0,
            Some("run and persist Anneal mistake replay and regression closure".to_owned()),
        );
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

fn failed(tier: Tier, threshold: f32, error: &SynapseCalyxError) -> TierResult {
    TierResult::new(
        tier,
        false,
        0.0,
        threshold,
        Some(error.remediation.to_owned()),
    )
}
