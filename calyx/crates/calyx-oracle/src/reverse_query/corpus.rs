use std::collections::{BTreeMap, BTreeSet, HashMap};

use calyx_aster::cf::{ColumnFamily, recurrence_prefix_range};
use calyx_aster::mvcc::{Freshness, Snapshot};
use calyx_aster::recurrence::{StoredRecurrenceRow, decode_recurrence_row};
use calyx_aster::vault::{AsterVault, encode};
use calyx_core::{AnchorValue, Clock, Constellation, CxId};
use serde::Serialize;

use crate::evidence_error;
use crate::{
    DomainId, ORACLE_DOMAIN_METADATA_KEY, ORACLE_FALLBACK_DOMAIN_METADATA_KEY, OracleError,
};

use super::reverse_query_context::ReverseContext;
use super::{
    ORACLE_EFFECT_METADATA_KEY, action_from_constellation, answer_label, structural_confidence,
};

const BASE_SCAN_PAGE_ROWS: usize = 1024;

#[derive(Clone, Debug)]
pub(super) struct ReverseCorpus {
    by_outcome: BTreeMap<String, Vec<OccurrenceEdge>>,
    by_action: HashMap<String, Vec<OccurrenceEdge>>,
    structural_by_answer: BTreeMap<String, Vec<StructuralEdge>>,
    stats: ReverseStats,
}

impl ReverseCorpus {
    pub(super) fn load<C>(vault: &AsterVault<C>, domain: &DomainId) -> Result<Self, OracleError>
    where
        C: Clock,
    {
        vault.with_scoped_latest_snapshot(
            Freshness::FreshDerived,
            evidence_error::ORACLE_CORPUS_READER_LEASE_MS,
            OracleError::from,
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
        let mut corpus = Self {
            by_outcome: BTreeMap::new(),
            by_action: HashMap::new(),
            structural_by_answer: BTreeMap::new(),
            stats: ReverseStats {
                snapshot_seq: snapshot.seq(),
                base_scans: 1,
                ..ReverseStats::default()
            },
        };

        vault
            .scan_cf_pages_snapshot(
                snapshot,
                ColumnFamily::Base,
                BASE_SCAN_PAGE_ROWS,
                |rows| -> Result<(), evidence_error::ScanError> {
                    corpus.stats.base_rows_scanned += rows.len() as u64;
                    for (_, bytes) in rows {
                        let cx = encode::decode_constellation_base(&bytes)
                            .map_err(|_| evidence_error::corrupt(domain, "base constellation"))?;
                        if !matches_domain(&cx, domain) {
                            continue;
                        }
                        corpus.stats.domain_rows_scanned += 1;
                        corpus.collect_structural_edges(&cx, domain)?;
                        corpus.collect_recurrence_edges(vault, snapshot, &cx, domain)?;
                    }
                    Ok(())
                },
            )
            .map_err(|error| evidence_error::scan_read(error, domain, "scan base corpus"))?;
        Ok(corpus)
    }

    pub(super) fn stats(&self) -> ReverseStats {
        self.stats.clone()
    }

    pub(super) fn recurrence_edges(&self, answer_label: &str) -> &[OccurrenceEdge] {
        self.by_outcome
            .get(answer_label)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn structural_edges(&self, answer_label: &str) -> &[StructuralEdge] {
        self.structural_by_answer
            .get(answer_label)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn action_counts(&self, action_id: &str) -> ActionCounts {
        let mut counts = ActionCounts::default();
        for edge in self.by_action.get(action_id).into_iter().flatten() {
            counts.add(edge.grounded);
        }
        counts
    }

    pub(super) fn action_answer_counts(&self, action_id: &str, answer_label: &str) -> ActionCounts {
        let mut counts = ActionCounts::default();
        for edge in self.recurrence_edges(answer_label) {
            if edge.action_id == action_id {
                counts.add(edge.grounded);
            }
        }
        counts
    }

    fn collect_structural_edges(
        &mut self,
        cx: &Constellation,
        domain: &DomainId,
    ) -> Result<(), OracleError> {
        let Some(action) = action_from_constellation(cx) else {
            return Ok(());
        };
        let edge = StructuralEdge {
            action_or_event: action,
            confidence: structural_confidence(cx),
        };
        for label in structural_answer_labels(cx, domain)? {
            self.structural_by_answer
                .entry(label)
                .or_default()
                .push(edge.clone());
        }
        Ok(())
    }

    fn collect_recurrence_edges<C>(
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
            let parsed: ReverseContext = serde_json::from_slice(&occurrence.context.bytes)
                .map_err(|_| evidence_error::corrupt(domain, "recurrence context"))?;
            let Some(action) = parsed
                .action()
                .or(base_action.as_deref())
                .filter(|value| !value.trim().is_empty())
            else {
                continue;
            };
            for edge in parsed.edges() {
                if edge.domain_id() != *domain {
                    continue;
                }
                let outcome_label = answer_label(edge.outcome(), domain)?;
                let occurrence_edge = OccurrenceEdge {
                    action_id: action.to_string(),
                    cx_id: cx.cx_id,
                    domain: edge.domain_id(),
                    grounded: edge.is_grounded(),
                };
                self.by_outcome
                    .entry(outcome_label)
                    .or_default()
                    .push(occurrence_edge.clone());
                self.by_action
                    .entry(occurrence_edge.action_id.clone())
                    .or_default()
                    .push(occurrence_edge);
            }
        }
        Ok(())
    }
}

fn structural_answer_labels(
    cx: &Constellation,
    domain: &DomainId,
) -> Result<BTreeSet<String>, OracleError> {
    let mut labels = BTreeSet::new();
    for anchor in &cx.anchors {
        labels.insert(answer_label(&anchor.value, domain)?);
    }
    if let Some(raw) = cx.metadata_value(ORACLE_EFFECT_METADATA_KEY) {
        if let Ok(value) = serde_json::from_str::<AnchorValue>(raw) {
            labels.insert(answer_label(&value, domain)?);
        }
        labels.insert(answer_label(&AnchorValue::Text(raw.to_string()), domain)?);
        labels.insert(answer_label(&AnchorValue::Enum(raw.to_string()), domain)?);
    }
    Ok(labels)
}

fn matches_domain(cx: &Constellation, domain: &DomainId) -> bool {
    cx.metadata_value(ORACLE_DOMAIN_METADATA_KEY) == Some(domain.as_str())
        || cx.metadata_value(ORACLE_FALLBACK_DOMAIN_METADATA_KEY) == Some(domain.as_str())
}

#[derive(Clone, Debug)]
pub(super) struct OccurrenceEdge {
    pub(super) action_id: String,
    pub(super) cx_id: CxId,
    pub(super) domain: DomainId,
    pub(super) grounded: bool,
}

#[derive(Clone, Debug)]
pub(super) struct StructuralEdge {
    pub(super) action_or_event: String,
    pub(super) confidence: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(super) struct ReverseStats {
    pub(super) snapshot_seq: u64,
    pub(super) base_scans: u64,
    pub(super) walk_calls: u64,
    pub(super) base_rows_scanned: u64,
    pub(super) domain_rows_scanned: u64,
    pub(super) recurrence_range_scans: u64,
    pub(super) recurrence_rows_scanned: u64,
    pub(super) matched_edges: u64,
    pub(super) structural_matches: u64,
    pub(super) grounded_causes_observed: u64,
    pub(super) provisional_causes_observed: u64,
    pub(super) expanded_actions: u64,
    pub(super) memo_hits: u64,
    pub(super) cycle_skips: u64,
    pub(super) depth_prunes: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ActionGroup {
    pub(super) cx_ids: BTreeSet<CxId>,
    pub(super) domain: Option<DomainId>,
    pub(super) grounded_count: u64,
    pub(super) provisional_count: u64,
}

impl ActionGroup {
    pub(super) fn add(&mut self, edge: &OccurrenceEdge) {
        self.cx_ids.insert(edge.cx_id);
        self.domain.get_or_insert_with(|| edge.domain.clone());
        if edge.grounded {
            self.grounded_count += 1;
        } else {
            self.provisional_count += 1;
        }
    }

    pub(super) fn domain(&self, fallback: &DomainId) -> DomainId {
        self.domain.clone().unwrap_or_else(|| fallback.clone())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ActionCounts {
    pub(super) grounded: u64,
    pub(super) provisional: u64,
}

impl ActionCounts {
    fn add(&mut self, grounded: bool) {
        if grounded {
            self.grounded += 1;
        } else {
            self.provisional += 1;
        }
    }

    pub(super) fn total(self) -> u64 {
        self.grounded.saturating_add(self.provisional)
    }
}
