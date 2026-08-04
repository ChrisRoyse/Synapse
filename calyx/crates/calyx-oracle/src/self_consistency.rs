//! Oracle self-consistency measured from grounded recurrence streams.

use std::collections::BTreeMap;

use calyx_assay::{MIN_ASSAY_SAMPLES, entropy_bits};
use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, LedgerRef, content_address};
use calyx_ledger::{ActorId, EntryKind, SubjectId};
use serde::Serialize;

use crate::evidence::{ObservationRow, OracleEvidence};
use crate::{DomainId, OracleError, OracleSelfConsistency};

pub const ORACLE_DOMAIN_METADATA_KEY: &str = "oracle.domain";
pub const ORACLE_FALLBACK_DOMAIN_METADATA_KEY: &str = "domain";
pub const MIN_FLAKINESS_PAIRS: u64 = 10;
pub const MIN_VALIDITY_SAMPLES: usize = MIN_ASSAY_SAMPLES;

const LEDGER_ACTOR: &str = "calyx-oracle";
const LEDGER_TAG: &str = "oracle_self_consistency_v1";

pub fn oracle_self_consistency<C>(
    vault: &AsterVault<C>,
    domain: DomainId,
    clock: &dyn Clock,
) -> Result<OracleSelfConsistency, OracleError>
where
    C: Clock,
{
    let evidence = OracleEvidence::load(vault, &domain)?;
    oracle_self_consistency_from_evidence(vault, &domain, &evidence, clock)
}

/// Measures grounded Oracle self-consistency without appending an audit row.
/// Read-only health observation must never advance the vault.
pub fn oracle_self_consistency_read_only<C>(
    vault: &AsterVault<C>,
    domain: DomainId,
) -> Result<OracleSelfConsistency, OracleError>
where
    C: Clock,
{
    let evidence = OracleEvidence::load(vault, &domain)?;
    let stats = consistency_stats(&domain, &evidence)?;
    Ok(OracleSelfConsistency::with_provenance(
        stats.flakiness,
        stats.validity,
        stats.provisional,
        None,
    ))
}

pub(crate) fn oracle_self_consistency_from_evidence<C>(
    vault: &AsterVault<C>,
    domain: &DomainId,
    evidence: &OracleEvidence,
    clock: &dyn Clock,
) -> Result<OracleSelfConsistency, OracleError>
where
    C: Clock,
{
    let stats = consistency_stats(domain, evidence)?;
    let mut result = OracleSelfConsistency::with_provenance(
        stats.flakiness,
        stats.validity,
        stats.provisional,
        None,
    );
    let provenance = write_ledger(vault, domain, &stats, &result, clock)?;
    result.provenance = Some(provenance);
    Ok(result)
}

fn consistency_stats(
    domain: &DomainId,
    evidence: &OracleEvidence,
) -> Result<ConsistencyStats, OracleError> {
    if evidence.stats.domain_rows_scanned == 0 {
        return Err(OracleError::DomainNotFound);
    }
    let mut total_pairs = 0_u64;
    let mut agreement_pairs = 0_u64;
    let mut validity_samples = Vec::new();
    let mut by_cx = BTreeMap::<_, Vec<&ObservationRow>>::new();
    for observation in &evidence.observations {
        by_cx
            .entry(observation.cx_id)
            .or_default()
            .push(observation);
    }

    for observations in by_cx.values() {
        let mut counts = BTreeMap::<String, u64>::new();
        for observation in observations {
            let Some(verdict) = &observation.outcome_label else {
                continue;
            };
            *counts.entry(verdict.clone()).or_default() += 1;
            if let Some(truth) = &observation.ground_truth_label {
                validity_samples.push(ValiditySample {
                    verdict: verdict.clone(),
                    ground_truth: truth.clone(),
                });
            }
        }
        let n = observations.len() as u64;
        total_pairs += pair_count(n);
        agreement_pairs += counts.values().map(|count| pair_count(*count)).sum::<u64>();
    }

    if total_pairs < MIN_FLAKINESS_PAIRS {
        return Err(OracleError::NoRecurrence {
            domain: domain.clone(),
        });
    }

    let flakiness = 1.0 - (agreement_pairs as f32 / total_pairs as f32);
    let (validity, provisional) = validity(domain, &validity_samples)?;
    Ok(ConsistencyStats {
        pair_count: total_pairs,
        agreement_pairs,
        validity_samples: validity_samples.len(),
        flakiness: flakiness.clamp(0.0, 1.0),
        validity: validity.clamp(0.0, 1.0),
        provisional,
    })
}

fn validity(_domain: &DomainId, samples: &[ValiditySample]) -> Result<(f32, bool), OracleError> {
    if samples.is_empty() {
        return Ok((0.0, true));
    }
    if samples.len() < MIN_VALIDITY_SAMPLES {
        return Ok((0.0, true));
    }
    if samples
        .iter()
        .all(|sample| sample.verdict == sample.ground_truth)
    {
        return Ok((1.0, false));
    }

    let truth_codes = label_codes(samples.iter().map(|sample| &sample.ground_truth));
    let entropy = entropy_bits(&truth_codes);
    if entropy <= f32::EPSILON {
        let matches = samples
            .iter()
            .filter(|sample| sample.verdict == sample.ground_truth)
            .count();
        return Ok((matches as f32 / samples.len() as f32, false));
    }

    let verdict_codes = label_codes(samples.iter().map(|sample| &sample.verdict));
    let estimate = discrete_mutual_information_bits(&verdict_codes, &truth_codes);
    Ok(((estimate / entropy).clamp(0.0, 1.0), false))
}

fn write_ledger<C>(
    vault: &AsterVault<C>,
    domain: &DomainId,
    stats: &ConsistencyStats,
    result: &OracleSelfConsistency,
    clock: &dyn Clock,
) -> Result<LedgerRef, OracleError>
where
    C: Clock,
{
    let digest = domain_digest(domain);
    let payload = MeasurementPayload::new(domain, stats, result, clock.now());
    let bytes = serde_json::to_vec(&payload).map_err(|_| OracleError::LedgerWriteFailure)?;
    vault
        .append_ledger_entry(
            EntryKind::Assay,
            SubjectId::Query(digest.to_vec()),
            bytes,
            ActorId::Service(LEDGER_ACTOR.to_string()),
        )
        .map_err(|_| OracleError::LedgerWriteFailure)
}

#[derive(Clone, Debug, PartialEq)]
struct ValiditySample {
    verdict: String,
    ground_truth: String,
}

#[derive(Clone, Debug, PartialEq)]
struct ConsistencyStats {
    pair_count: u64,
    agreement_pairs: u64,
    validity_samples: usize,
    flakiness: f32,
    validity: f32,
    provisional: bool,
}

#[derive(Clone, Debug, Serialize)]
struct MeasurementPayload {
    tag: &'static str,
    domain_id: String,
    pair_count: u64,
    agreement_pairs: u64,
    validity_samples: u64,
    flakiness: f32,
    validity: f32,
    ceiling: f32,
    provisional: bool,
    ts: u64,
}

impl MeasurementPayload {
    fn new(
        domain: &DomainId,
        stats: &ConsistencyStats,
        result: &OracleSelfConsistency,
        ts: u64,
    ) -> Self {
        Self {
            tag: LEDGER_TAG,
            domain_id: hex_bytes(&domain_digest(domain)),
            pair_count: stats.pair_count,
            agreement_pairs: stats.agreement_pairs,
            validity_samples: stats.validity_samples as u64,
            flakiness: stats.flakiness,
            validity: stats.validity,
            ceiling: result.ceiling,
            provisional: stats.provisional,
            ts,
        }
    }
}

fn pair_count(n: u64) -> u64 {
    n.saturating_mul(n.saturating_sub(1)) / 2
}

fn label_codes<'a>(labels: impl Iterator<Item = &'a String>) -> Vec<usize> {
    let mut index = BTreeMap::new();
    let mut out = Vec::new();
    for label in labels {
        let next = index.len();
        out.push(*index.entry(label.clone()).or_insert(next));
    }
    out
}

fn discrete_mutual_information_bits(left: &[usize], right: &[usize]) -> f32 {
    debug_assert_eq!(left.len(), right.len());
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut left_counts = BTreeMap::<usize, u64>::new();
    let mut right_counts = BTreeMap::<usize, u64>::new();
    let mut joint_counts = BTreeMap::<(usize, usize), u64>::new();
    for (&left_code, &right_code) in left.iter().zip(right) {
        *left_counts.entry(left_code).or_default() += 1;
        *right_counts.entry(right_code).or_default() += 1;
        *joint_counts.entry((left_code, right_code)).or_default() += 1;
    }
    let total = left.len() as f64;
    joint_counts
        .into_iter()
        .map(|((left_code, right_code), joint_count)| {
            let p_xy = joint_count as f64 / total;
            let p_x = left_counts[&left_code] as f64 / total;
            let p_y = right_counts[&right_code] as f64 / total;
            p_xy * (p_xy / (p_x * p_y)).log2()
        })
        .sum::<f64>() as f32
}

fn domain_digest(domain: &DomainId) -> [u8; 16] {
    content_address([domain.as_str().as_bytes()])
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
