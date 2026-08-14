use std::collections::BTreeMap;

use calyx_aster::cf::{ColumnFamily, recurrence_prefix_range};
use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::recurrence::{StoredRecurrenceRow, decode_recurrence_row};
use calyx_aster::vault::{AsterVault, encode};
use calyx_core::{Clock, Constellation};

use super::{ChildCandidate, ChildKey, context::ExpansionContext, outcome_label};
use crate::evidence_error;
use crate::{
    DomainId, ORACLE_ACTION_METADATA_KEY, ORACLE_DOMAIN_METADATA_KEY,
    ORACLE_FALLBACK_DOMAIN_METADATA_KEY, OracleError,
};

const BASE_SCAN_PAGE_ROWS: usize = 1024;

pub(super) struct DomainCorpus {
    children_by_action: BTreeMap<String, Vec<ChildCandidate>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DomainCorpusStats {
    pub(super) base_rows_scanned: u64,
    pub(super) recurrence_rows_scanned: u64,
}

impl DomainCorpus {
    pub(super) fn load<C>(
        vault: &AsterVault<C>,
        domain: &DomainId,
    ) -> Result<(Self, DomainCorpusStats), OracleError>
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
    ) -> Result<(Self, DomainCorpusStats), OracleError>
    where
        C: Clock,
    {
        let mut stats = DomainCorpusStats::default();
        let mut grouped = BTreeMap::<String, BTreeMap<ChildKey, ChildCandidate>>::new();
        vault
            .scan_cf_pages_snapshot(
                snapshot,
                ColumnFamily::Base,
                BASE_SCAN_PAGE_ROWS,
                |rows| -> Result<(), evidence_error::ScanError> {
                    stats.base_rows_scanned += rows.len() as u64;
                    for (_, bytes) in rows {
                        let cx = encode::decode_constellation_base(&bytes)
                            .map_err(|_| evidence_error::corrupt(domain, "base constellation"))?;
                        if matches_domain(&cx, domain) {
                            collect_children(
                                vault,
                                snapshot,
                                &cx,
                                domain,
                                &mut stats,
                                &mut grouped,
                            )?;
                        }
                    }
                    Ok(())
                },
            )
            .map_err(|error| evidence_error::scan_read(error, domain, "scan base corpus"))?;
        Ok((
            Self {
                children_by_action: grouped
                    .into_iter()
                    .map(|(action, by_key)| (action, finalize_children(by_key)))
                    .collect(),
            },
            stats,
        ))
    }

    pub(super) fn children_for(&self, action: &str) -> &[ChildCandidate] {
        self.children_by_action
            .get(action)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn collect_children<C>(
    vault: &AsterVault<C>,
    snapshot: Snapshot,
    cx: &Constellation,
    domain: &DomainId,
    stats: &mut DomainCorpusStats,
    grouped: &mut BTreeMap<String, BTreeMap<ChildKey, ChildCandidate>>,
) -> Result<(), OracleError>
where
    C: Clock,
{
    let rows = vault
        .scan_cf_range_snapshot(
            snapshot,
            ColumnFamily::Recurrence,
            &recurrence_prefix_range(cx.cx_id),
        )
        .map_err(|error| evidence_error::recurrence_read(error, domain))?;
    let mut occurrences = Vec::new();
    for (_, value) in rows {
        if let StoredRecurrenceRow::Occurrence(occurrence) = decode_recurrence_row(&value)
            .map_err(|error| evidence_error::recurrence_read(error, domain))?
        {
            stats.recurrence_rows_scanned += 1;
            occurrences.push(occurrence);
        }
    }
    occurrences.sort_by_key(|occurrence| (occurrence.t_k, occurrence.id));
    let base_action = action_from_constellation(cx);
    for occurrence in occurrences {
        if occurrence.context.bytes.is_empty() {
            continue;
        }
        let parsed: ExpansionContext = serde_json::from_slice(&occurrence.context.bytes)
            .map_err(|_| evidence_error::corrupt(domain, "recurrence context"))?;
        let Some(action) = parsed
            .action()
            .map(ToOwned::to_owned)
            .or_else(|| base_action.clone())
        else {
            continue;
        };
        for candidate in parsed.consequences() {
            let label = outcome_label(&candidate.outcome)
                .map_err(|_| evidence_error::corrupt(domain, "consequence outcome"))?;
            let key = ChildKey {
                domain: candidate.domain.as_str().to_string(),
                action_or_event: candidate.action_or_event.clone(),
                outcome_label: label,
            };
            grouped
                .entry(action.clone())
                .or_default()
                .entry(key)
                .and_modify(|stored| stored.add_evidence(candidate.grounded))
                .or_insert(candidate);
        }
    }
    Ok(())
}

fn finalize_children(by_key: BTreeMap<ChildKey, ChildCandidate>) -> Vec<ChildCandidate> {
    let total = by_key
        .values()
        .map(|candidate| candidate.evidence_count)
        .sum::<u64>()
        .max(1);
    by_key
        .into_values()
        .map(|mut candidate| {
            candidate.predicted_count = total;
            candidate
        })
        .collect()
}

fn matches_domain(cx: &Constellation, domain: &DomainId) -> bool {
    cx.metadata_value(ORACLE_DOMAIN_METADATA_KEY) == Some(domain.as_str())
        || cx.metadata_value(ORACLE_FALLBACK_DOMAIN_METADATA_KEY) == Some(domain.as_str())
}

fn action_from_constellation(cx: &Constellation) -> Option<String> {
    const ORACLE_FALLBACK_ACTION_METADATA_KEY: &str = "action";
    cx.metadata_value(ORACLE_ACTION_METADATA_KEY)
        .or_else(|| cx.metadata_value(ORACLE_FALLBACK_ACTION_METADATA_KEY))
        .map(ToOwned::to_owned)
}
