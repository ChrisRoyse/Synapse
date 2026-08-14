//! Snapshot-scoped Oracle evidence loaded once per query.

use calyx_assay::AssayStore;
use calyx_aster::cf::{ColumnFamily, recurrence_prefix_range};
use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::recurrence::{StoredRecurrenceRow, decode_recurrence_row};
use calyx_aster::vault::{AsterVault, encode};
use calyx_core::{AnchorValue, CalyxError, Clock, Constellation, CxId, Result as CalyxResult};
use serde::{Deserialize, Serialize};

use crate::evidence_error;
use crate::{
    DomainId, ORACLE_ACTION_METADATA_KEY, ORACLE_DOMAIN_METADATA_KEY,
    ORACLE_FALLBACK_DOMAIN_METADATA_KEY, OracleError,
};

const ORACLE_FALLBACK_ACTION_METADATA_KEY: &str = "action";
const BASE_SCAN_PAGE_ROWS: usize = 1024;

#[derive(Clone, Debug)]
pub(crate) struct OracleEvidence {
    pub(crate) snapshot: u64,
    pub(crate) assay: AssayStore,
    pub(crate) observations: Vec<ObservationRow>,
    pub(crate) stats: OracleEvidenceStats,
}

impl OracleEvidence {
    pub(crate) fn load<C>(vault: &AsterVault<C>, domain: &DomainId) -> Result<Self, OracleError>
    where
        C: Clock,
    {
        vault.with_scoped_latest_snapshot(
            Freshness::FreshDerived,
            evidence_error::ORACLE_CORPUS_READER_LEASE_MS,
            |snapshot| Self::load_snapshot(vault, domain, snapshot),
        )
    }

    fn load_snapshot<C>(
        vault: &AsterVault<C>,
        domain: &DomainId,
        snapshot: Snapshot,
    ) -> Result<Self, OracleError>
    where
        C: Clock,
    {
        let assay =
            AssayStore::load_from_vault_snapshot(vault, snapshot).map_err(OracleError::from)?;
        let mut evidence = Self {
            snapshot: snapshot.seq(),
            assay,
            observations: Vec::new(),
            stats: OracleEvidenceStats {
                assay_scans: 1,
                base_scans: 1,
                ..OracleEvidenceStats::default()
            },
        };

        vault
            .scan_cf_pages_snapshot(
                snapshot,
                ColumnFamily::Base,
                BASE_SCAN_PAGE_ROWS,
                |rows| -> Result<(), evidence_error::ScanError> {
                    evidence.stats.base_rows_scanned += rows.len() as u64;
                    for (_, bytes) in rows {
                        let cx = encode::decode_constellation_base(&bytes)
                            .map_err(|_| evidence_error::corrupt(domain, "base constellation"))?;
                        if !matches_domain(&cx, domain) {
                            continue;
                        }
                        evidence.stats.domain_rows_scanned += 1;
                        evidence.collect_recurrence(vault, snapshot, &cx, domain)?;
                    }
                    Ok(())
                },
            )
            .map_err(|error| evidence_error::scan_read(error, domain, "scan base corpus"))?;
        Ok(evidence)
    }

    fn collect_recurrence<C>(
        &mut self,
        vault: &AsterVault<C>,
        snapshot: Snapshot,
        cx: &Constellation,
        domain: &DomainId,
    ) -> Result<(), OracleError>
    where
        C: Clock,
    {
        let range = recurrence_prefix_range(cx.cx_id);
        let rows = vault
            .scan_cf_range_snapshot(snapshot, ColumnFamily::Recurrence, &range)
            .map_err(|error| evidence_error::recurrence_read(error, domain))?;
        self.stats.recurrence_range_scans += 1;
        self.stats.recurrence_rows_scanned += rows.len() as u64;
        let base_action = action_from_constellation(cx);

        for (_, value) in rows {
            let StoredRecurrenceRow::Occurrence(occurrence) = decode_recurrence_row(&value)
                .map_err(|error| evidence_error::recurrence_read(error, domain))?
            else {
                continue;
            };
            if occurrence.context.bytes.is_empty() {
                continue;
            }
            let parsed: EvidenceContext = serde_json::from_slice(&occurrence.context.bytes)
                .map_err(|_| evidence_error::corrupt(domain, "recurrence context"))?;
            self.observations.push(ObservationRow {
                cx_id: cx.cx_id,
                action_id: parsed
                    .action()
                    .map(ToOwned::to_owned)
                    .or_else(|| base_action.clone()),
                outcome: parsed.outcome(),
                outcome_label: parsed
                    .outcome_label()
                    .map_err(|_| evidence_error::corrupt(domain, "outcome label"))?,
                ground_truth_label: parsed
                    .ground_truth_label()
                    .map_err(|_| evidence_error::corrupt(domain, "ground truth label"))?,
                consequences: parsed.consequences(),
            });
        }
        Ok(())
    }
}

fn matches_domain(cx: &Constellation, domain: &DomainId) -> bool {
    cx.metadata_value(ORACLE_DOMAIN_METADATA_KEY) == Some(domain.as_str())
        || cx.metadata_value(ORACLE_FALLBACK_DOMAIN_METADATA_KEY) == Some(domain.as_str())
}

fn action_from_constellation(cx: &Constellation) -> Option<String> {
    cx.metadata_value(ORACLE_ACTION_METADATA_KEY)
        .or_else(|| cx.metadata_value(ORACLE_FALLBACK_ACTION_METADATA_KEY))
        .map(ToOwned::to_owned)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ObservationRow {
    pub(crate) cx_id: CxId,
    pub(crate) action_id: Option<String>,
    pub(crate) outcome: Option<AnchorValue>,
    pub(crate) outcome_label: Option<String>,
    pub(crate) ground_truth_label: Option<String>,
    pub(crate) consequences: Vec<ConsequenceSeed>,
}

impl ObservationRow {
    pub(crate) fn matches_action(&self, expected: &str) -> bool {
        self.action_id.as_deref() == Some(expected)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConsequenceSeed {
    pub(crate) action_or_event: String,
    pub(crate) domain: String,
    pub(crate) outcome: AnchorValue,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct OracleEvidenceStats {
    pub(crate) assay_scans: u64,
    pub(crate) base_scans: u64,
    pub(crate) base_rows_scanned: u64,
    pub(crate) domain_rows_scanned: u64,
    pub(crate) recurrence_range_scans: u64,
    pub(crate) recurrence_rows_scanned: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct EvidenceContext {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    action_id: Option<String>,
    #[serde(default, rename = "oracle_verdict")]
    oracle_verdict: Option<AnchorEvidence>,
    #[serde(default, rename = "outcome_anchor")]
    outcome_anchor: Option<AnchorEvidence>,
    #[serde(default, rename = "ground_truth_anchor")]
    ground_truth_anchor: Option<AnchorEvidence>,
    #[serde(default)]
    consequence: Option<ConsequenceEvidence>,
    #[serde(default)]
    consequences: Vec<ConsequenceEvidence>,
}

impl EvidenceContext {
    fn action(&self) -> Option<&str> {
        self.action_id.as_deref().or(self.action.as_deref())
    }

    fn outcome(&self) -> Option<AnchorValue> {
        self.outcome_anchor
            .as_ref()
            .or(self.oracle_verdict.as_ref())
            .map(|evidence| evidence.value.clone())
    }

    fn outcome_label(&self) -> CalyxResult<Option<String>> {
        self.outcome_anchor
            .as_ref()
            .or(self.oracle_verdict.as_ref())
            .map(AnchorEvidence::label)
            .transpose()
    }

    fn ground_truth_label(&self) -> CalyxResult<Option<String>> {
        self.ground_truth_anchor
            .as_ref()
            .map(AnchorEvidence::label)
            .transpose()
    }

    fn consequences(&self) -> Vec<ConsequenceSeed> {
        self.consequence
            .iter()
            .chain(self.consequences.iter())
            .filter(|consequence| !consequence.action_or_event.trim().is_empty())
            .map(|consequence| ConsequenceSeed {
                action_or_event: consequence.action_or_event.clone(),
                domain: consequence.domain.clone(),
                outcome: consequence.outcome.value.clone(),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
struct AnchorEvidence {
    value: AnchorValue,
}

impl AnchorEvidence {
    fn label(&self) -> CalyxResult<String> {
        serde_json::to_string(&self.value).map_err(|error| {
            CalyxError::aster_corrupt_shard(format!("anchor label encode: {error}"))
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ConsequenceEvidence {
    action_or_event: String,
    #[serde(default = "default_consequence_domain")]
    domain: String,
    outcome: AnchorEvidence,
}

fn default_consequence_domain() -> String {
    "oracle".to_string()
}
