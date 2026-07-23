mod plan;
mod sketch;

mod cuda;

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{CalyxError, Result, SlotId};

#[cfg(not(feature = "cuda"))]
use crate::cuda_strict::cuda_unavailable;
use crate::cuda_strict::strict_cuda_requested;
use crate::ksg::MIN_ASSAY_SAMPLES;
use crate::nmi::partitioned_histogram_nmi;

use self::cuda::ensemble_redundancy_from_lenses_cuda_strict_impl;

use super::model::{
    ENSEMBLE_CARD_SCHEMA_VERSION, EnsembleCard, EnsembleLensInput, EnsemblePairRedundancyEvidence,
    EnsembleRedundancyEvidence, EnsembleRedundancyMethod,
};

pub use plan::{
    DEFAULT_LINEAR_CKA_SEED, LINEAR_CKA_JACKKNIFE_BLOCKS, LINEAR_CKA_TUPLES_PER_ROW,
    LinearCkaTuplePlan, MAX_LINEAR_CKA_TUPLES, MIN_LINEAR_CKA_TUPLES, linear_cka_tuple_plan,
};
pub use sketch::{LinearCkaSketch, linear_cka_sketch_from_row_fn, linear_cka_sketch_from_rows};

pub const LINEAR_CKA_REDUNDANCY_METHOD: &str = "debiased_linear_cka_hsic1_u4_v1";
const EXACT_TUPLE_DESIGN: &str = "complete_four_subset_enumeration_v1";
const SAMPLED_TUPLE_DESIGN: &str = "blake3_counter_uniform_four_distinct_with_replacement_v1";
const EXACT_UNCERTAINTY_METHOD: &str = "none_complete_tuple_population";
const SAMPLED_UNCERTAINTY_METHOD: &str = "delete_32_group_jackknife_ratio_v1";
const GATE_SCORE_METHOD: &str = "max_0_raw_plus_4_mc_se_clamped_1_fail_closed_v1";

#[derive(Clone, Debug)]
pub struct EnsembleRedundancySketchInput {
    name: String,
    slot: SlotId,
    nmi_signature: Vec<f32>,
    linear_cka: LinearCkaSketch,
}

impl EnsembleRedundancySketchInput {
    pub fn new(
        name: impl Into<String>,
        slot: SlotId,
        nmi_signature: Vec<f32>,
        linear_cka: LinearCkaSketch,
    ) -> Self {
        Self {
            name: name.into(),
            slot,
            nmi_signature,
            linear_cka,
        }
    }
}

pub fn ensemble_redundancy_from_lenses(
    lenses: &[EnsembleLensInput],
    nmi_bins: usize,
) -> Result<EnsembleRedundancyEvidence> {
    if strict_cuda_requested() {
        return ensemble_redundancy_from_lenses_cuda_strict(lenses, nmi_bins);
    }
    let row_count = lenses.first().map(|lens| lens.vectors.len()).unwrap_or(0);
    let plan = linear_cka_tuple_plan(row_count)?;
    let mut sketches = Vec::with_capacity(lenses.len());
    for lens in lenses {
        let linear_cka = linear_cka_sketch_from_rows(&plan, &lens.vectors)?;
        sketches.push(EnsembleRedundancySketchInput::new(
            lens.name.clone(),
            lens.slot,
            row_signature(&lens.vectors)?,
            linear_cka,
        ));
    }
    ensemble_redundancy_from_sketches(&plan, &sketches, nmi_bins)
}

pub fn ensemble_redundancy_from_lenses_cuda_strict(
    lenses: &[EnsembleLensInput],
    nmi_bins: usize,
) -> Result<EnsembleRedundancyEvidence> {
    ensemble_redundancy_from_lenses_cuda_strict_impl(lenses, nmi_bins)
}

pub fn ensemble_redundancy_from_sketches(
    plan: &LinearCkaTuplePlan,
    lenses: &[EnsembleRedundancySketchInput],
    nmi_bins: usize,
) -> Result<EnsembleRedundancyEvidence> {
    validate_sketch_inputs(plan, lenses)?;
    let mut pairs = Vec::new();
    for a in 0..lenses.len() {
        for b in (a + 1)..lenses.len() {
            let linear_cka = sketch::estimate_pair(
                &lenses[a].linear_cka,
                &lenses[b].linear_cka,
                plan.is_exact(),
            )?;
            let nmi = partitioned_histogram_nmi(
                &lenses[a].nmi_signature,
                &lenses[b].nmi_signature,
                nmi_bins,
            )?
            .nmi;
            pairs.push(EnsemblePairRedundancyEvidence {
                a: lenses[a].name.clone(),
                b: lenses[b].name.clone(),
                slot_a: lenses[a].slot,
                slot_b: lenses[b].slot,
                linear_cka,
                nmi,
            });
        }
    }
    Ok(EnsembleRedundancyEvidence {
        method: redundancy_method(plan),
        pairs,
    })
}

pub(super) fn validate_evidence(
    lenses: &[EnsembleLensInput],
    evidence: &EnsembleRedundancyEvidence,
) -> Result<()> {
    let roster = lenses
        .iter()
        .map(|lens| (lens.slot, lens.name.as_str()))
        .collect::<BTreeMap<_, _>>();
    validate_evidence_for_roster(&roster, evidence)
}

/// Validates the redundancy provenance required at persisted-card trust boundaries.
///
/// Serde keeps legacy cards decodable for diagnostic migration, but evidence consumers must require
/// the current schema so relabeling malformed evidence as legacy cannot bypass validation.
pub fn validate_ensemble_card_redundancy(card: &EnsembleCard) -> Result<()> {
    if card.schema_version != ENSEMBLE_CARD_SCHEMA_VERSION {
        return Err(CalyxError::assay_degenerate_input(format!(
            "unsupported EnsembleCard schema {}; expected {ENSEMBLE_CARD_SCHEMA_VERSION}",
            card.schema_version
        )));
    }
    let method = card.redundancy_method.clone().ok_or_else(|| {
        CalyxError::assay_degenerate_input(
            "current-schema EnsembleCard is missing redundancy method metadata",
        )
    })?;
    let mut roster = BTreeMap::new();
    let mut names = BTreeSet::new();
    let conditioning_roster = card
        .lenses
        .iter()
        .map(|lens| (lens.slot, lens.name.as_str()))
        .collect::<Vec<_>>();
    for lens in &card.lenses {
        if roster.insert(lens.slot, lens.name.as_str()).is_some()
            || !names.insert(lens.name.as_str())
        {
            return Err(CalyxError::assay_degenerate_input(
                "EnsembleCard lens names and slots must be unique",
            ));
        }
    }
    crate::logistic::validate_conditioning_provenance(
        &card.conditioning,
        &conditioning_roster,
        card.n_samples,
    )?;
    let mut pairs = Vec::with_capacity(card.pairs.len());
    for pair in &card.pairs {
        let linear_cka = pair.redundancy.clone().ok_or_else(|| {
            CalyxError::assay_degenerate_input(format!(
                "current-schema EnsembleCard pair {}:{} is missing redundancy evidence",
                pair.slot_a, pair.slot_b
            ))
        })?;
        if !pair.corr.is_finite() || (pair.corr - linear_cka.mc_gate_upper_estimate).abs() > 1.0e-6
        {
            return Err(CalyxError::assay_degenerate_input(format!(
                "EnsembleCard pair {}:{} corr {} != redundancy gate {}",
                pair.slot_a, pair.slot_b, pair.corr, linear_cka.mc_gate_upper_estimate
            )));
        }
        pairs.push(EnsemblePairRedundancyEvidence {
            a: pair.a.clone(),
            b: pair.b.clone(),
            slot_a: pair.slot_a,
            slot_b: pair.slot_b,
            linear_cka,
            nmi: pair.nmi,
        });
    }
    validate_evidence_for_roster(&roster, &EnsembleRedundancyEvidence { method, pairs })
}

fn validate_evidence_for_roster(
    roster: &BTreeMap<SlotId, &str>,
    evidence: &EnsembleRedundancyEvidence,
) -> Result<()> {
    validate_redundancy_method_metadata(&evidence.method)?;
    let expected_pairs = roster.len().saturating_sub(1) * roster.len() / 2;
    if evidence.pairs.len() != expected_pairs {
        return Err(CalyxError::assay_degenerate_input(format!(
            "ensemble redundancy pairs {} != expected {expected_pairs}",
            evidence.pairs.len()
        )));
    }
    let mut pair_keys = BTreeSet::new();
    for pair in &evidence.pairs {
        let Some(expected_a) = roster.get(&pair.slot_a) else {
            return Err(CalyxError::assay_degenerate_input(format!(
                "redundancy evidence has unknown slot {}",
                pair.slot_a
            )));
        };
        let Some(expected_b) = roster.get(&pair.slot_b) else {
            return Err(CalyxError::assay_degenerate_input(format!(
                "redundancy evidence has unknown slot {}",
                pair.slot_b
            )));
        };
        if pair.slot_a == pair.slot_b || pair.a != *expected_a || pair.b != *expected_b {
            return Err(CalyxError::assay_degenerate_input(
                "redundancy evidence pair names do not match its slots",
            ));
        }
        let key = if pair.slot_a < pair.slot_b {
            (pair.slot_a, pair.slot_b)
        } else {
            (pair.slot_b, pair.slot_a)
        };
        if !pair_keys.insert(key) {
            return Err(CalyxError::assay_degenerate_input(
                "redundancy evidence contains a duplicate pair",
            ));
        }
        validate_pair(pair)?;
    }
    Ok(())
}

pub fn validate_redundancy_method_metadata(method: &EnsembleRedundancyMethod) -> Result<()> {
    let (tuple_design, uncertainty_method, uncertainty_blocks) = if method.exact {
        (EXACT_TUPLE_DESIGN, EXACT_UNCERTAINTY_METHOD, 0)
    } else {
        (
            SAMPLED_TUPLE_DESIGN,
            SAMPLED_UNCERTAINTY_METHOD,
            LINEAR_CKA_JACKKNIFE_BLOCKS,
        )
    };
    let valid = method.metric == LINEAR_CKA_REDUNDANCY_METHOD
        && method.tuple_design == tuple_design
        && method.row_count >= MIN_ASSAY_SAMPLES
        && method.tuple_count > 0
        && valid_seed_hex(&method.seed_hex)
        && valid_blake3(&method.tuple_plan_blake3)
        && method.uncertainty_method == uncertainty_method
        && method.uncertainty_blocks == uncertainty_blocks
        && method.gate_score_method == GATE_SCORE_METHOD;
    if !valid {
        return Err(CalyxError::assay_degenerate_input(
            "ensemble redundancy method metadata is incomplete or unsupported",
        ));
    }
    Ok(())
}

fn valid_seed_hex(seed: &str) -> bool {
    seed.strip_prefix("0x")
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_blake3(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_pair(pair: &EnsemblePairRedundancyEvidence) -> Result<()> {
    let estimate = &pair.linear_cka;
    let expected_point = estimate.raw_signed_point.max(0.0);
    let valid = estimate.raw_signed_point.is_finite()
        && (-1.0..=1.0).contains(&estimate.raw_signed_point)
        && estimate.redundancy_point.is_finite()
        && (0.0..=1.0).contains(&estimate.redundancy_point)
        && (estimate.redundancy_point - expected_point).abs() <= 1.0e-5
        && estimate.mc_standard_error.is_finite()
        && estimate.mc_standard_error >= 0.0
        && estimate.mc_gate_upper_estimate.is_finite()
        && (estimate.redundancy_point..=1.0).contains(&estimate.mc_gate_upper_estimate)
        && pair.nmi.is_finite()
        && (0.0..=1.0).contains(&pair.nmi);
    if !valid {
        return Err(CalyxError::assay_degenerate_input(format!(
            "invalid redundancy evidence for {} and {}",
            pair.a, pair.b
        )));
    }
    Ok(())
}

fn validate_sketch_inputs(
    plan: &LinearCkaTuplePlan,
    lenses: &[EnsembleRedundancySketchInput],
) -> Result<()> {
    if plan.row_count() < MIN_ASSAY_SAMPLES {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "ensemble redundancy requires at least {MIN_ASSAY_SAMPLES} rows; got {}",
            plan.row_count()
        )));
    }
    let mut slots = BTreeSet::new();
    let mut names = BTreeSet::new();
    for lens in lenses {
        if !slots.insert(lens.slot) || !names.insert(lens.name.as_str()) {
            return Err(CalyxError::assay_insufficient_samples(
                "ensemble redundancy lens names and slots must be unique",
            ));
        }
        if !lens.linear_cka.matches(plan) {
            return Err(CalyxError::assay_degenerate_input(format!(
                "lens {} was not sketched with the shared tuple plan",
                lens.name
            )));
        }
        if lens.nmi_signature.len() != plan.row_count() {
            return Err(CalyxError::assay_insufficient_samples(format!(
                "lens {} NMI signature rows {} != {}",
                lens.name,
                lens.nmi_signature.len(),
                plan.row_count()
            )));
        }
    }
    Ok(())
}

fn redundancy_method(plan: &LinearCkaTuplePlan) -> EnsembleRedundancyMethod {
    EnsembleRedundancyMethod {
        metric: LINEAR_CKA_REDUNDANCY_METHOD.to_string(),
        tuple_design: if plan.is_exact() {
            EXACT_TUPLE_DESIGN
        } else {
            SAMPLED_TUPLE_DESIGN
        }
        .to_string(),
        row_count: plan.row_count(),
        tuple_count: plan.tuple_count(),
        seed_hex: format!("0x{:016x}", plan.seed()),
        tuple_plan_blake3: plan.digest_hex(),
        exact: plan.is_exact(),
        uncertainty_method: if plan.is_exact() {
            EXACT_UNCERTAINTY_METHOD
        } else {
            SAMPLED_UNCERTAINTY_METHOD
        }
        .to_string(),
        uncertainty_blocks: if plan.is_exact() {
            0
        } else {
            LINEAR_CKA_JACKKNIFE_BLOCKS
        },
        gate_score_method: GATE_SCORE_METHOD.to_string(),
    }
}

fn row_signature(rows: &[Vec<f32>]) -> Result<Vec<f32>> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            if row.is_empty() || row.iter().any(|value| !value.is_finite()) {
                return Err(CalyxError::assay_degenerate_input(format!(
                    "ensemble NMI signature row {index} is empty or non-finite"
                )));
            }
            Ok((row.iter().map(|value| f64::from(*value)).sum::<f64>() / row.len() as f64) as f32)
        })
        .collect()
}
